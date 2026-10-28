//! Repositories: the `Repository` type, `repositories`, `repository(name:)`,
//! `updateMirrorRepository` and the key-value metadata mutations.
//!
//! Readable by signed-in users (and anonymous users when `auth.public` is on), filtered by
//! repository permissions: a repository the viewer may not read is absent from lists
//! and counts and resolves to `null`, exactly like a missing one. Fields that reveal
//! infrastructure (`mirrorInfo.remoteURL`, `mirrorInfo.shard`, `externalServices`) are for site
//! admins only. Writing metadata needs the `REPO_METADATA#WRITE` permission and read access to
//! the repository.

use async_graphql::{Context, Enum, ID, Object, Result, Scalar, ScalarType, SimpleObject, Value};
use chrono::{DateTime, Utc};
use sourcegull_db::{ListReposOptions, RepoOrder, RepoUpdatePriority, RepoUpdateReason};
use sourcegull_services::relay::marshal_id;
use sourcegull_types::{CloneStatus, ExternalServiceKind, GitserverRepo, Repo, RepoId};

use crate::EmptyResponse;
use crate::context::{
    access_error, codes, db_error, error, page_size, repo_access, require_site_admin,
    require_viewer, services, to_int,
};
use crate::external_services::{
    ExternalServiceConnection, ExternalServiceKindGql, ExternalServiceNode,
    parse_external_service_id,
};
use crate::git::{
    self, GitCommitNode, GitRefConnection, GitRefNode, GitRefOrder, GitRefType, RepoHandle,
    RepositoryComparisonNode,
};
use crate::pagination::{PageInfo, decode_cursor};

pub use sourcegull_graphql_core::ids::{REPOSITORY_KIND, parse_repository_id};

/// A 64-bit integer, serialized as a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BigInt(pub i64);

#[Scalar]
impl ScalarType for BigInt {
    fn parse(value: Value) -> async_graphql::InputValueResult<Self> {
        match &value {
            Value::String(s) => Ok(Self(s.parse()?)),
            Value::Number(n) => n
                .as_i64()
                .map(Self)
                .ok_or_else(|| async_graphql::InputValueError::custom("not an integer")),
            _ => Err(async_graphql::InputValueError::expected_type(value)),
        }
    }

    fn to_value(&self) -> Value {
        Value::String(self.0.to_string())
    }
}

pub(crate) use sourcegull_graphql_core::repo::require_reader;

/// A repository, with its repo node state when already loaded.
#[derive(Debug, Clone)]
pub struct RepositoryNode {
    pub repo: Repo,
    pub gitserver: Option<GitserverRepo>,
}

impl RepositoryNode {
    async fn gitserver(&self, ctx: &Context<'_>) -> Result<Option<GitserverRepo>> {
        if self.gitserver.is_some() {
            return Ok(self.gitserver.clone());
        }
        services(ctx)?
            .db
            .gitserver_repos()
            .get(self.repo.id)
            .await
            .map_err(db_error)
    }
}

pub use sourcegull_graphql_core::codehost::ExternalLink;

/// The repository's identity on its code host.
#[derive(SimpleObject)]
pub struct ExternalRepository {
    /// The code host's id for the repository (the GitLab project id).
    pub id: String,
    /// The code host type, e.g. `gitlab`.
    pub service_type: String,
    /// The code host instance, e.g. `https://gitlab.com/`.
    #[graphql(name = "serviceID")]
    pub service_id: String,
}

/// A repository key-value pair (a null value is a tag).
#[derive(SimpleObject)]
pub struct KeyValuePair {
    pub key: String,
    pub value: Option<String>,
}

#[Object(name = "Repository")]
impl RepositoryNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(REPOSITORY_KIND, i64::from(self.repo.id.get())))
    }

    /// The CODEOWNERS file a site admin ingested for this repository, if any.
    async fn ingested_codeowners(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<sourcegull_own_graphql::ownership::CodeownersIngestedFileNode>> {
        sourcegull_own_graphql::ownership::ingested_for_repo(ctx, self.repo.id).await
    }

    /// The repository name, e.g. `gitlab.com/group/project`.
    async fn name(&self) -> &str {
        &self.repo.name
    }

    /// `{host}/{pathWithNamespace}`.
    async fn uri(&self) -> &str {
        &self.repo.uri
    }

    /// The repository's page in Sourcegull.
    async fn url(&self) -> String {
        format!("/{}", self.repo.name)
    }

    /// The description from the code host.
    async fn description(&self) -> &str {
        &self.repo.description
    }

    /// Links to the repository on its code host.
    #[graphql(name = "externalURLs")]
    async fn external_urls(&self) -> Vec<ExternalLink> {
        if self.repo.url.is_empty() {
            return Vec::new();
        }
        let kind = ExternalServiceKind::ALL
            .into_iter()
            .find(|k| k.service_type() == self.repo.external_service_type)
            .map(ExternalServiceKindGql::from);
        vec![ExternalLink {
            url: self.repo.url.clone(),
            service_kind: kind,
            service_type: Some(self.repo.external_service_type.clone()),
        }]
    }

    /// Whether the repository is a fork.
    async fn is_fork(&self) -> bool {
        self.repo.fork
    }

    /// Whether the repository is archived on the code host.
    async fn is_archived(&self) -> bool {
        self.repo.archived
    }

    /// Whether the repository is not visible to everyone on the code host.
    async fn is_private(&self) -> bool {
        self.repo.private
    }

    /// The code host's star count.
    async fn stars(&self) -> i32 {
        self.repo.stars
    }

    /// Topics from the code host.
    async fn topics(&self) -> &[String] {
        &self.repo.topics
    }

    /// When the code host last saw activity.
    async fn last_activity_at(&self) -> Option<DateTime<Utc>> {
        self.repo.last_activity_at
    }

    /// When the repository was added to Sourcegull.
    async fn created_at(&self) -> DateTime<Utc> {
        self.repo.created_at
    }

    /// When the repository's metadata last changed.
    async fn updated_at(&self) -> DateTime<Utc> {
        self.repo.updated_at
    }

    /// The repository's identity on its code host.
    async fn external_repository(&self) -> ExternalRepository {
        ExternalRepository {
            id: self.repo.external_id.clone(),
            service_type: self.repo.external_service_type.clone(),
            service_id: self.repo.external_service_id.clone(),
        }
    }

    /// Mirroring state on the repo node.
    async fn mirror_info(&self, ctx: &Context<'_>) -> Result<MirrorRepositoryInfo> {
        Ok(MirrorRepositoryInfo {
            repo: self.repo.clone(),
            gitserver: self.gitserver(ctx).await?,
        })
    }

    /// The repository's text search index (site admins only).
    async fn text_search_index(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<crate::repos_admin::TextSearchIndexNode>> {
        crate::repos_admin::text_search_index(ctx, &self.repo).await
    }

    /// Key-value metadata.
    async fn key_value_pairs(&self, ctx: &Context<'_>) -> Result<Vec<KeyValuePair>> {
        let kvps = services(ctx)?
            .db
            .repos()
            .kvps(self.repo.id)
            .await
            .map_err(db_error)?;
        Ok(kvps
            .into_iter()
            .map(|(key, value)| KeyValuePair { key, value })
            .collect())
    }

    /// The repository's permission state; null when none was ever recorded (site admins only).
    async fn permissions_info(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<crate::permissions::PermissionsInfo>> {
        crate::permissions::repository_permissions_info(ctx, self.repo.id).await
    }

    /// The users granted this repository (site admins only). Unrestricted and public
    /// repositories are readable by everyone without being listed.
    async fn authorized_users(
        &self,
        ctx: &Context<'_>,
        first: i32,
        after: Option<String>,
    ) -> Result<crate::users::UserConnection> {
        crate::permissions::repository_authorized_users(ctx, self.repo.id, first, after).await
    }

    /// The code host connections that mirror this repository (site admins only).
    async fn external_services(&self, ctx: &Context<'_>) -> Result<ExternalServiceConnection> {
        require_site_admin(ctx)?;
        let db = &services(ctx)?.db;
        let mut nodes = Vec::new();
        for (id, _) in db.repos().sources(self.repo.id).await.map_err(db_error)? {
            if let Some(svc) = db.external_services(None).get(id).await.map_err(db_error)? {
                nodes.push(ExternalServiceNode::new(svc));
            }
        }
        Ok(ExternalServiceConnection {
            total_count: to_int(i64::try_from(nodes.len()).unwrap_or(i64::MAX)),
            nodes,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        })
    }

    /// The commit `rev` resolves to (any git revision; empty means HEAD), or null if it does
    /// not exist. Fails with `REPO_NOT_CLONED` / `CLONE_IN_PROGRESS` while the repository is
    /// not on its repo node.
    async fn commit(
        &self,
        ctx: &Context<'_>,
        rev: String,
        #[graphql(name = "inputRevspec", desc = "Accepted for compatibility.")]
        input_revspec: Option<String>,
    ) -> Result<Option<GitCommitNode>> {
        let _ = input_revspec;
        require_reader(ctx)?;
        RepoHandle::new(ctx, &self.repo)?
            .resolve_commit(ctx, &rev)
            .await
    }

    /// The branches or tags a code intelligence policy pattern names, or the commit it
    /// names, newest first.
    async fn preview_git_object_filter(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "type")]
        object_type: sourcegull_codeintel_graphql::codeintel_policies::GitObjectType,
        pattern: String,
        first: Option<i32>,
        count_objects_younger_than_hours: Option<i32>,
    ) -> Result<sourcegull_codeintel_graphql::codeintel_policies::GitObjectFilterPreview> {
        sourcegull_codeintel_graphql::codeintel_policies::preview_git_object_filter(
            ctx,
            &self.repo,
            object_type,
            &pattern,
            first,
            count_objects_younger_than_hours,
        )
        .await
    }

    /// The repository's auto-indexing configuration: the saved JSON configuration
    /// and what inference gives. Fails while auto-indexing is not enabled.
    async fn index_configuration(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<sourcegull_codeintel_graphql::autoindex::IndexConfigurationNode>> {
        require_reader(ctx)?;
        sourcegull_codeintel_graphql::autoindex::index_configuration(ctx, &self.repo).await
    }

    /// The branch HEAD points to (null while the repository is not cloned).
    async fn default_branch(&self, ctx: &Context<'_>) -> Result<Option<GitRefNode>> {
        require_reader(ctx)?;
        git::default_branch(&RepoHandle::new(ctx, &self.repo)?).await
    }

    /// Branches, most recently committed first by default.
    async fn branches(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        #[graphql(default)] order_by: GitRefOrder,
    ) -> Result<GitRefConnection> {
        self.git_refs(
            ctx,
            Some(GitRefType::GitBranch),
            first,
            after,
            query,
            order_by,
        )
        .await
    }

    /// Tags, most recently committed first by default.
    async fn tags(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        #[graphql(default)] order_by: GitRefOrder,
    ) -> Result<GitRefConnection> {
        self.git_refs(ctx, Some(GitRefType::GitTag), first, after, query, order_by)
            .await
    }

    /// Branches and/or tags.
    async fn git_refs(
        &self,
        ctx: &Context<'_>,
        #[graphql(name = "type")] ref_type: Option<GitRefType>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        #[graphql(default)] order_by: GitRefOrder,
    ) -> Result<GitRefConnection> {
        require_reader(ctx)?;
        git::list_refs(
            &RepoHandle::new(ctx, &self.repo)?,
            ref_type,
            first,
            after.as_deref(),
            query.as_deref(),
            order_by,
        )
        .await
    }

    /// Compares `base...head` (empty or omitted means HEAD): the commits in `base..head` and
    /// the changes from their merge base to `head`.
    async fn comparison(
        &self,
        ctx: &Context<'_>,
        base: Option<String>,
        head: Option<String>,
    ) -> Result<RepositoryComparisonNode> {
        require_reader(ctx)?;
        git::comparison(&RepoHandle::new(ctx, &self.repo)?, base, head).await
    }
}

/// A repository's mirroring state (`gitserver_repos`).
pub struct MirrorRepositoryInfo {
    repo: Repo,
    gitserver: Option<GitserverRepo>,
}

#[Object]
impl MirrorRepositoryInfo {
    /// The URL the repository is cloned from, without credentials (site admins only).
    #[graphql(name = "remoteURL")]
    async fn remote_url(&self, ctx: &Context<'_>) -> Result<String> {
        require_site_admin(ctx)?;
        let sources = services(ctx)?
            .db
            .repos()
            .sources(self.repo.id)
            .await
            .map_err(db_error)?;
        Ok(sources
            .into_iter()
            .next()
            .map(|(_, url)| url)
            .unwrap_or_default())
    }

    /// Whether the repository is on disk.
    async fn cloned(&self) -> bool {
        self.status() == CloneStatus::Cloned
    }

    /// Whether the repository is cloned but has no commits: a git repository without any
    /// branch yet, or a code host project without a repository the repo node can fetch
    /// ([`remote_unavailable`](Self::remote_unavailable)). None of these is an error.
    async fn is_empty(&self) -> bool {
        self.gitserver.as_ref().is_some_and(|g| {
            g.clone_status == CloneStatus::Cloned
                && (g.remote_unavailable.is_some() || g.head_oid.is_none())
        })
    }

    /// Whether the code host has no git repository for the project: none yet (GitLab
    /// `empty_repo`: created without a README and never pushed to) or its repository feature
    /// is disabled. The repo node keeps an empty copy and picks up the repository on a later
    /// fetch.
    async fn remote_missing(&self) -> bool {
        self.unavailable()
            .is_some_and(sourcegull_types::RemoteUnavailable::no_repository)
    }

    /// Why the code host has no repository the repo node can fetch, if so.
    async fn remote_unavailable(&self) -> Option<RemoteUnavailableReason> {
        self.unavailable().map(Into::into)
    }

    /// Whether the code host token may see the project but not download its code (GitLab:
    /// below Reporter on a private project). Not a failed clone.
    async fn no_code_access(&self) -> bool {
        self.unavailable() == Some(sourcegull_types::RemoteUnavailable::NoCodeAccess)
    }

    /// Whether the initial clone is running.
    async fn clone_in_progress(&self) -> bool {
        self.status() == CloneStatus::Cloning
    }

    /// Progress output of a running clone.
    async fn clone_progress(&self) -> Option<&str> {
        self.gitserver.as_ref()?.cloning_progress.as_deref()
    }

    /// Whether the repo node found the repository corrupt.
    async fn is_corrupted(&self) -> bool {
        self.gitserver
            .as_ref()
            .is_some_and(|g| g.corrupted_at.is_some())
    }

    /// When the repository was found corrupt, with the reason. Sourcegull keeps only the
    /// current finding (cleared by the re-clone), so the list has at most one entry.
    async fn corruption_logs(&self) -> Vec<RepoCorruptionLog> {
        self.gitserver
            .as_ref()
            .and_then(|g| {
                Some(RepoCorruptionLog {
                    timestamp: g.corrupted_at?,
                    reason: g.last_error.clone().unwrap_or_default(),
                })
            })
            .into_iter()
            .collect()
    }

    /// When the repo node next fetches the repository on its own schedule.
    async fn next_sync_at(&self, ctx: &Context<'_>) -> Result<Option<DateTime<Utc>>> {
        let schedule = services(ctx)?
            .db
            .gitserver_repos()
            .fetch_schedule(self.repo.id)
            .await
            .map_err(db_error)?;
        Ok(schedule.and_then(|s| s.next_fetch_at))
    }

    /// When the last fetch finished.
    async fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.gitserver.as_ref()?.last_fetched
    }

    /// When the last fetch changed a ref.
    async fn last_changed(&self) -> Option<DateTime<Utc>> {
        self.gitserver.as_ref()?.last_changed
    }

    /// The error of the last clone or fetch.
    async fn last_error(&self) -> Option<&str> {
        self.gitserver.as_ref()?.last_error.as_deref()
    }

    /// Size on disk in bytes.
    async fn byte_size(&self) -> BigInt {
        BigInt(
            self.gitserver
                .as_ref()
                .and_then(|g| g.repo_size_bytes)
                .unwrap_or(0),
        )
    }

    /// The repo node holding the repository (site admins only).
    async fn shard(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        require_site_admin(ctx)?;
        Ok(self
            .gitserver
            .as_ref()
            .and_then(|g| g.node.clone())
            .or_else(|| self.repo.shard.clone()))
    }
}

impl MirrorRepositoryInfo {
    fn status(&self) -> CloneStatus {
        self.gitserver
            .as_ref()
            .map_or(CloneStatus::NotCloned, |g| g.clone_status)
    }

    fn unavailable(&self) -> Option<sourcegull_types::RemoteUnavailable> {
        self.gitserver.as_ref().and_then(|g| g.remote_unavailable)
    }
}

/// Why a code host project has no repository the repo node can fetch.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum RemoteUnavailableReason {
    /// No repository yet (never pushed to).
    Missing,
    /// The project's repository feature is disabled on the code host.
    Disabled,
    /// The code host token may not download the project's code.
    NoCodeAccess,
}

impl From<sourcegull_types::RemoteUnavailable> for RemoteUnavailableReason {
    fn from(r: sourcegull_types::RemoteUnavailable) -> Self {
        match r {
            sourcegull_types::RemoteUnavailable::Missing => Self::Missing,
            sourcegull_types::RemoteUnavailable::Disabled => Self::Disabled,
            sourcegull_types::RemoteUnavailable::NoCodeAccess => Self::NoCodeAccess,
        }
    }
}

/// A corruption finding on a repository.
#[derive(SimpleObject)]
pub struct RepoCorruptionLog {
    /// When the repository was found corrupt.
    pub timestamp: DateTime<Utc>,
    /// Why.
    pub reason: String,
}

/// Filter on clone status.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "CloneStatus")]
pub enum CloneStatusGql {
    NotCloned,
    Cloning,
    Cloned,
}

/// Sort order of `repositories`.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum RepositoryOrderBy {
    /// By name.
    #[default]
    RepositoryName,
    /// By when the repository was added.
    RepoCreatedAt,
    /// By size on disk.
    Size,
}

/// A page of repositories.
#[derive(SimpleObject)]
pub struct RepositoryConnection {
    pub nodes: Vec<RepositoryNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Repository queries.
#[derive(Default)]
pub struct ReposQuery;

#[Object]
impl ReposQuery {
    /// Lists repositories.
    #[allow(clippy::too_many_arguments)]
    async fn repositories(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        #[graphql(desc = "Case-insensitive substring of the name.")] query: Option<String>,
        clone_status: Option<CloneStatusGql>,
        #[graphql(desc = "Only repositories whose last clone or fetch failed.")]
        failed_fetch: Option<bool>,
        #[graphql(
            default_with = "Some(false)",
            desc = "Only repositories their repo node found corrupt (site admins only)."
        )]
        corrupted: Option<bool>,
        #[graphql(
            default_with = "Some(true)",
            desc = "Include repositories that have a text search index."
        )]
        indexed: Option<bool>,
        #[graphql(
            default_with = "Some(true)",
            desc = "Include repositories that do not have a text search index."
        )]
        not_indexed: Option<bool>,
        #[graphql(default)] order_by: RepositoryOrderBy,
        #[graphql(default)] descending: bool,
        #[graphql(desc = "Only repositories mirrored by this code host connection.")]
        external_service: Option<ID>,
    ) -> Result<RepositoryConnection> {
        require_reader(ctx)?;
        // Explicit nulls mean the defaults.
        let corrupted = corrupted.unwrap_or(false);
        let indexed = indexed.unwrap_or(true);
        let not_indexed = not_indexed.unwrap_or(true);
        // Corruption and index state are infrastructure details.
        if corrupted || !indexed || !not_indexed {
            require_site_admin(ctx)?;
        }
        let db = &services(ctx)?.db;
        let limit = page_size(first, 50, 1000);
        let offset = decode_cursor(after.as_deref())?;
        if !indexed && !not_indexed {
            return Ok(RepositoryConnection {
                nodes: Vec::new(),
                total_count: 0,
                page_info: PageInfo::new(offset, limit, 0),
            });
        }
        let opts = ListReposOptions {
            query: query.filter(|q| !q.trim().is_empty()),
            clone_status: clone_status.map(|c| match c {
                CloneStatusGql::NotCloned => CloneStatus::NotCloned,
                CloneStatusGql::Cloning => CloneStatus::Cloning,
                CloneStatusGql::Cloned => CloneStatus::Cloned,
            }),
            failed_fetch: failed_fetch.unwrap_or(false),
            corrupted,
            indexed: (indexed != not_indexed).then_some(indexed),
            external_service_id: external_service
                .as_ref()
                .map(|id| parse_external_service_id(id))
                .transpose()?,
            ids: None,
            access: Some(repo_access(ctx).await?),
            order: match order_by {
                RepositoryOrderBy::RepositoryName => RepoOrder::Name,
                RepositoryOrderBy::RepoCreatedAt => RepoOrder::CreatedAt,
                RepositoryOrderBy::Size => RepoOrder::Size,
            },
            descending,
            limit: limit + 1,
            offset,
        };
        // `opts.access` is the viewer's.
        #[allow(clippy::disallowed_methods)]
        let repos = db.repos().list(&opts).await.map_err(db_error)?;
        // `opts.access` is the viewer's.
        #[allow(clippy::disallowed_methods)]
        let total = db.repos().count(&opts).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, repos.len());
        let repos: Vec<Repo> = repos
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .collect();
        let ids: Vec<RepoId> = repos.iter().map(|r| r.id).collect();
        let mut gitserver = db
            .gitserver_repos()
            .get_many(&ids)
            .await
            .map_err(db_error)?;
        let nodes = repos
            .into_iter()
            .map(|repo| {
                let gs = gitserver
                    .iter()
                    .position(|g| g.repo_id == repo.id)
                    .map(|i| gitserver.swap_remove(i));
                RepositoryNode {
                    repo,
                    gitserver: gs,
                }
            })
            .collect();
        Ok(RepositoryConnection {
            nodes,
            total_count: to_int(total),
            page_info,
        })
    }

    /// Looks up a repository by name (case-insensitive).
    async fn repository(&self, ctx: &Context<'_>, name: String) -> Result<Option<RepositoryNode>> {
        require_reader(ctx)?;
        let access = repo_access(ctx).await?;
        let repo = services(ctx)?
            .db
            .repos()
            .get_by_name_authorized(&name, &access)
            .await
            .map_err(db_error)?;
        Ok(repo.map(|repo| RepositoryNode {
            repo,
            gitserver: None,
        }))
    }
}

/// Repository mutations.
#[derive(Default)]
pub struct ReposMutation;

#[Object]
impl ReposMutation {
    /// Asks the repo node to fetch the repository now (site admins only).
    async fn update_mirror_repository(
        &self,
        ctx: &Context<'_>,
        repository: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let db = &services(ctx)?.db;
        let id = parse_repository_id(&repository)?;
        // Site admins only.
        #[allow(clippy::disallowed_methods)]
        if db.repos().get(id).await.map_err(db_error)?.is_none() {
            return Err(error(codes::NOT_FOUND, "Repository not found."));
        }
        db.repo_update_jobs()
            .enqueue(id, RepoUpdatePriority::IMMEDIATE, RepoUpdateReason::Manual)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Adds a key-value pair to a repository; a null value makes the key a tag.
    async fn add_repo_key_value_pair(
        &self,
        ctx: &Context<'_>,
        repo: ID,
        key: String,
        value: Option<String>,
    ) -> Result<EmptyResponse> {
        let id = metadata_repo(ctx, &repo, &key).await?;
        let added = services(ctx)?
            .db
            .repos()
            .add_kvp(id, &key, value.as_deref())
            .await
            .map_err(db_error)?;
        if !added {
            return Err(error(
                codes::ALREADY_EXISTS,
                format!("The repository already has the key {key:?}."),
            ));
        }
        Ok(EmptyResponse)
    }

    /// Replaces the value of a repository's key; a null value makes the key a tag.
    async fn update_repo_key_value_pair(
        &self,
        ctx: &Context<'_>,
        repo: ID,
        key: String,
        value: Option<String>,
    ) -> Result<EmptyResponse> {
        let id = metadata_repo(ctx, &repo, &key).await?;
        let updated = services(ctx)?
            .db
            .repos()
            .update_kvp(id, &key, value.as_deref())
            .await
            .map_err(db_error)?;
        if !updated {
            return Err(error(codes::NOT_FOUND, "Key not found."));
        }
        Ok(EmptyResponse)
    }

    /// Deletes a key-value pair from a repository.
    async fn delete_repo_key_value_pair(
        &self,
        ctx: &Context<'_>,
        repo: ID,
        key: String,
    ) -> Result<EmptyResponse> {
        let id = metadata_repo(ctx, &repo, &key).await?;
        let deleted = services(ctx)?
            .db
            .repos()
            .delete_kvp(id, &key)
            .await
            .map_err(db_error)?;
        if !deleted {
            return Err(error(codes::NOT_FOUND, "Key not found."));
        }
        Ok(EmptyResponse)
    }
}

/// The repository a metadata mutation writes: the viewer needs `REPO_METADATA#WRITE` and read
/// access (an unreadable repository is not found), and the key must not be empty.
async fn metadata_repo(ctx: &Context<'_>, repo: &ID, key: &str) -> Result<RepoId> {
    let actor = require_viewer(ctx)?;
    sourcegull_authn::access::require_permission(
        actor,
        sourcegull_authn::Permission::RepoMetadataWrite,
    )
    .map_err(access_error)?;
    if key.trim().is_empty() {
        return Err(error(codes::INVALID_INPUT, "The key must not be empty."));
    }
    let id = parse_repository_id(repo)?;
    sourcegull_graphql_core::repo::load_readable(ctx, i64::from(id.get()))
        .await?
        .map(|r| r.id)
        .ok_or_else(|| error(codes::NOT_FOUND, "Repository not found."))
}

/// Loads a live repository for `node(id:)`, honouring the read rule.
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<RepositoryNode>> {
    let repo = sourcegull_graphql_core::repo::load_readable(ctx, id).await?;
    Ok(repo.map(|repo| RepositoryNode {
        repo,
        gitserver: None,
    }))
}
