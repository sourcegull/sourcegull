//! Code intelligence configuration policies over GraphQL:
//! `codeIntelligenceConfigurationPolicies`, the create, update and delete mutations,
//! `previewRepositoryFilter` and `Repository.previewGitObjectFilter`, and
//! `PreciseIndex.retentionPolicyOverview`.
//!
//! Anyone who can read repositories sees the policies (a policy for one repository only when
//! they can read it); creating, editing, deleting and previewing repository patterns need a
//! site admin. Embeddings are not part of Sourcegull: `embeddingsEnabled` is always false and
//! can't be turned on.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_codeintel_policies::fields::{check_policy_fields, compile_globs};
use sourcegull_codeintel_policies::store::{
    ConfigurationPolicy, GitObjectType as DbObjectType, ListPolicies, PolicyFields, PolicyStore,
    PolicyWriteError,
};
use sourcegull_codeintel_policies::{PolicyError, RepoGit, Retention};
use sourcegull_codeintel_scip::store::Upload;
use sourcegull_db::ListReposOptions;
use sourcegull_services::relay::{marshal_id, unmarshal_id};
use sourcegull_types::glob::Glob;
use sourcegull_types::{RepoAccess, RepoId};

use crate::context::{
    codes, db_error, error, page_size, repo_access, require_site_admin, services, to_int,
    validation,
};
use crate::pagination::{PageInfo, decode_cursor};
use crate::precise_indexes::CodeIntelRepository;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::declared;
use sourcegull_graphql_core::ids::parse_repository_id;
use sourcegull_graphql_core::repo::{load_readable, require_reader};

/// Node kind of policy IDs.
pub const POLICY_KIND: &str = "CodeIntelligenceConfigurationPolicy";
const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;
const DEFAULT_PREVIEW: i64 = 15;

/// `GitObjectType`. Policies name commits, branches (`GIT_TREE`) or tags.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
#[allow(clippy::enum_variant_names)] // the API's names
pub enum GitObjectType {
    GitCommit,
    GitTag,
    GitTree,
    GitBlob,
    GitUnknown,
}

impl GitObjectType {
    fn of(t: DbObjectType) -> Self {
        match t {
            DbObjectType::Commit => Self::GitCommit,
            DbObjectType::Branch => Self::GitTree,
            DbObjectType::Tag => Self::GitTag,
        }
    }

    fn to_db(self) -> Option<DbObjectType> {
        match self {
            Self::GitCommit => Some(DbObjectType::Commit),
            Self::GitTree => Some(DbObjectType::Branch),
            Self::GitTag => Some(DbObjectType::Tag),
            Self::GitBlob | Self::GitUnknown => None,
        }
    }
}

/// `CodeIntelligenceConfigurationPolicy`.
#[derive(Debug, Clone)]
pub struct PolicyNode(pub ConfigurationPolicy);

#[Object(name = "CodeIntelligenceConfigurationPolicy")]
impl PolicyNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(POLICY_KIND, i64::from(self.0.id)))
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    /// The resources file entry that manages this policy (`codeIntelPolicies[<key>]`), or
    /// `codeIntelPolicies` when the file manages every policy not tied to one repository; such
    /// policies are read-only.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.0.repo_id.is_some() {
            return Ok(None);
        }
        declared::managed_by(
            services(ctx)?,
            declared::kinds::CODE_INTEL_POLICIES,
            &self.0.id.to_string(),
        )
        .await
    }

    /// The repository the policy applies to; null for policies on all repositories or on
    /// repository patterns.
    async fn repository(&self, ctx: &Context<'_>) -> Result<Option<CodeIntelRepository>> {
        let Some(repo) = self.0.repo_id else {
            return Ok(None);
        };
        Ok(load_readable(ctx, i64::from(repo.get()))
            .await?
            .map(CodeIntelRepository))
    }

    /// Globs on the names of the repositories the policy applies to.
    async fn repository_patterns(&self) -> Option<&[String]> {
        self.0.repository_patterns.as_deref()
    }

    #[graphql(name = "type")]
    async fn object_type(&self) -> GitObjectType {
        GitObjectType::of(self.0.object_type)
    }

    /// A glob on branch or tag names, or a commit.
    async fn pattern(&self) -> &str {
        &self.0.pattern
    }

    /// Built-in policies can be edited but not deleted.
    async fn protected(&self) -> bool {
        self.0.protected
    }

    async fn retention_enabled(&self) -> bool {
        self.0.retention_enabled
    }

    /// How long data visible from matching objects is kept after upload; null: forever.
    async fn retention_duration_hours(&self) -> Option<i32> {
        self.0.retention_duration_hours
    }

    /// For branches: also keep data for every commit on the branch, not only its tip.
    async fn retain_intermediate_commits(&self) -> bool {
        self.0.retain_intermediate_commits
    }

    async fn indexing_enabled(&self) -> bool {
        self.0.indexing_enabled
    }

    async fn syntactic_indexing_enabled(&self) -> Option<bool> {
        Some(self.0.syntactic_indexing_enabled)
    }

    /// The oldest commit (by commit date) auto-indexing indexes; null: any age.
    async fn index_commit_max_age_hours(&self) -> Option<i32> {
        self.0.index_commit_max_age_hours
    }

    async fn index_intermediate_commits(&self) -> bool {
        self.0.index_intermediate_commits
    }

    /// Always false.
    async fn embeddings_enabled(&self) -> bool {
        false
    }
}

/// A page of policies.
#[derive(SimpleObject)]
#[graphql(name = "CodeIntelligenceConfigurationPolicyConnection")]
pub struct PolicyConnection {
    nodes: Vec<PolicyNode>,
    total_count: Option<i32>,
    page_info: PageInfo,
}

/// `RepositoryFilterPreview`.
#[derive(SimpleObject)]
pub struct RepositoryFilterPreview {
    nodes: Vec<CodeIntelRepository>,
    /// Repositories in the result (all matches here: there is no limit on matches).
    total_count: i32,
    matches_all_repos: bool,
    limit: Option<i32>,
    total_matches: i32,
}

/// `CodeIntelGitObject`.
#[derive(SimpleObject)]
pub struct CodeIntelGitObject {
    name: String,
    rev: String,
    committed_at: DateTime<Utc>,
}

/// `GitObjectFilterPreview`.
#[derive(SimpleObject)]
pub struct GitObjectFilterPreview {
    nodes: Vec<CodeIntelGitObject>,
    total_count: i32,
    total_count_younger_than_threshold: Option<i32>,
}

/// `CodeIntelligenceRetentionPolicyMatch`.
#[derive(SimpleObject)]
pub struct CodeIntelligenceRetentionPolicyMatch {
    /// Null for the tip of the default branch, which always keeps the data it sees.
    configuration_policy: Option<PolicyNode>,
    matches: bool,
    /// Commits the policy names that the index serves, other than its own commit.
    protecting_commits: Option<Vec<String>>,
}

/// `CodeIntelligenceRetentionPolicyMatchesConnection`.
#[derive(SimpleObject)]
pub struct CodeIntelligenceRetentionPolicyMatchesConnection {
    nodes: Vec<CodeIntelligenceRetentionPolicyMatch>,
    total_count: Option<i32>,
    page_info: PageInfo,
}

impl CodeIntelligenceRetentionPolicyMatchesConnection {
    /// No matches (for an index job that has not uploaded an index yet).
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            total_count: Some(0),
            page_info: PageInfo::new(0, 0, 0),
        }
    }
}

/// Parses a policy ID.
pub fn parse_policy_id(id: &str) -> Option<i32> {
    let (kind, key) = unmarshal_id(id)?;
    (kind == POLICY_KIND).then(|| key.parse().ok()).flatten()
}

/// Loads a policy as a node if the viewer may see it.
pub async fn load_node(ctx: &Context<'_>, id: i32) -> Result<Option<PolicyNode>> {
    require_reader(ctx)?;
    let svc = services(ctx)?;
    let Some(p) = PolicyStore::new(&svc.db).get(id).await.map_err(db_error)? else {
        return Ok(None);
    };
    Ok(visible(ctx, &p, &repo_access(ctx).await?)
        .await?
        .then_some(PolicyNode(p)))
}

/// Whether the viewer may see `p`: policies for one repository need read access to it.
async fn visible(ctx: &Context<'_>, p: &ConfigurationPolicy, access: &RepoAccess) -> Result<bool> {
    let Some(repo) = p.repo_id else {
        return Ok(true);
    };
    Ok(services(ctx)?
        .db
        .repos()
        .get_authorized(repo, access)
        .await
        .map_err(db_error)?
        .is_some())
}

/// The repository behind an ID if the viewer can read it.
async fn readable_repo(
    ctx: &Context<'_>,
    id: &ID,
    access: &RepoAccess,
) -> Result<Option<sourcegull_types::Repo>> {
    let Ok(repo) = parse_repository_id(id) else {
        return Ok(None);
    };
    services(ctx)?
        .db
        .repos()
        .get_authorized(repo, access)
        .await
        .map_err(db_error)
}

pub fn policy_error(e: PolicyError) -> async_graphql::Error {
    match e {
        PolicyError::Git(e) => sourcegull_graphql_core::git::git_error(e),
        PolicyError::Db(e) => db_error(e),
    }
}

#[derive(Default)]
pub struct PoliciesQuery;

#[Object]
impl PoliciesQuery {
    /// Code intelligence configuration policies, protected ones first.
    #[allow(clippy::too_many_arguments)]
    async fn code_intelligence_configuration_policies(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only the policies that apply to this repository.")] repository: Option<
            ID,
        >,
        #[graphql(desc = "Matches the name.")] query: Option<String>,
        for_data_retention: Option<bool>,
        for_precise_indexing: Option<bool>,
        for_syntactic_indexing: Option<bool>,
        for_embeddings: Option<bool>,
        protected: Option<bool>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<PolicyConnection> {
        require_reader(ctx)?;
        let access = repo_access(ctx).await?;
        let empty = || PolicyConnection {
            nodes: Vec::new(),
            total_count: Some(0),
            page_info: PageInfo::new(0, 0, 0),
        };
        let repo = match &repository {
            None => None,
            Some(id) => match readable_repo(ctx, id, &access).await? {
                Some(r) => Some((r.id, r.name)),
                None => return Ok(empty()),
            },
        };
        if for_embeddings == Some(true) {
            return Ok(empty());
        }
        let all = PolicyStore::new(&services(ctx)?.db)
            .list(&ListPolicies {
                repo,
                query,
                for_retention: for_data_retention,
                for_indexing: for_precise_indexing,
                for_syntactic_indexing,
                protected,
            })
            .await
            .map_err(db_error)?;
        let mut shown = Vec::with_capacity(all.len());
        for p in all {
            if visible(ctx, &p, &access).await? {
                shown.push(p);
            }
        }
        let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let total = shown.len();
        let page: Vec<PolicyNode> = shown
            .into_iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(usize::try_from(limit + 1).unwrap_or(usize::MAX))
            .map(PolicyNode)
            .collect();
        let page_info = PageInfo::new(offset, limit, page.len());
        let mut nodes = page;
        nodes.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(PolicyConnection {
            nodes,
            total_count: Some(to_int(i64::try_from(total).unwrap_or(i64::MAX))),
            page_info,
        })
    }

    /// The repositories whose names match any of `patterns` (globs), for previewing a
    /// policy's repository patterns (site admins only).
    async fn preview_repository_filter(
        &self,
        ctx: &Context<'_>,
        patterns: Vec<String>,
        first: Option<i32>,
    ) -> Result<RepositoryFilterPreview> {
        require_site_admin(ctx)?;
        let globs = compile_globs(&patterns).map_err(validation)?;
        let limit = usize::try_from(page_size(first, DEFAULT_PREVIEW, MAX_PAGE)).unwrap_or(0);
        let repos = services(ctx)?.db.repos();
        let mut nodes = Vec::new();
        let (mut matched, mut total) = (0usize, 0usize);
        let mut opts = ListReposOptions {
            access: Some(RepoAccess::All),
            limit: 5000,
            ..ListReposOptions::default()
        };
        loop {
            // Site admins only: every repository.
            #[allow(clippy::disallowed_methods)]
            let page = repos.list(&opts).await.map_err(db_error)?;
            let n = page.len();
            for repo in page {
                total += 1;
                if globs.iter().any(|g| g.is_match(&repo.name)) {
                    matched += 1;
                    if nodes.len() < limit {
                        nodes.push(CodeIntelRepository(repo));
                    }
                }
            }
            if n < usize::try_from(opts.limit).unwrap_or(0) {
                break;
            }
            opts.offset += opts.limit;
        }
        let matched = i32::try_from(matched).unwrap_or(i32::MAX);
        Ok(RepositoryFilterPreview {
            nodes,
            total_count: matched,
            matches_all_repos: total > 0 && usize::try_from(matched).ok() == Some(total),
            limit: None,
            total_matches: matched,
        })
    }
}

/// `Repository.previewGitObjectFilter`: the branches or tags a pattern names, or the commit.
pub async fn preview_git_object_filter(
    ctx: &Context<'_>,
    repo: &sourcegull_types::Repo,
    object_type: GitObjectType,
    pattern: &str,
    first: Option<i32>,
    count_objects_younger_than_hours: Option<i32>,
) -> Result<GitObjectFilterPreview> {
    require_reader(ctx)?;
    let svc = services(ctx)?;
    let git = RepoGit::new(svc.repo_clients.clone(), repo.id, &repo.name);
    let limit = usize::try_from(page_size(first, DEFAULT_PREVIEW, MAX_PAGE)).unwrap_or(0);
    let mut objects: Vec<CodeIntelGitObject> = match object_type {
        GitObjectType::GitCommit => git
            .commit(pattern.trim())
            .await
            .map_err(policy_error)?
            .map(|(rev, committed_at)| CodeIntelGitObject {
                name: rev.clone(),
                rev,
                committed_at,
            })
            .into_iter()
            .collect(),
        GitObjectType::GitTree | GitObjectType::GitTag => {
            let glob =
                Glob::new(pattern).map_err(|e| error(codes::INVALID_INPUT, e.to_string()))?;
            let kind = if object_type == GitObjectType::GitTree {
                sourcegull_codeintel_policies::git::RefKind::Branch
            } else {
                sourcegull_codeintel_policies::git::RefKind::Tag
            };
            git.refs()
                .await
                .map_err(policy_error)?
                .into_iter()
                .filter(|r| r.kind == kind && glob.is_match(&r.name))
                .map(|r| CodeIntelGitObject {
                    name: r.name,
                    rev: r.commit,
                    committed_at: r.committed_at,
                })
                .collect()
        }
        GitObjectType::GitBlob | GitObjectType::GitUnknown => Vec::new(),
    };
    objects.sort_by(|a, b| {
        b.committed_at
            .cmp(&a.committed_at)
            .then(a.name.cmp(&b.name))
    });
    let younger = count_objects_younger_than_hours.map(|h| {
        let since = Utc::now() - chrono::TimeDelta::hours(i64::from(h));
        to_int(
            i64::try_from(objects.iter().filter(|o| o.committed_at > since).count())
                .unwrap_or(i64::MAX),
        )
    });
    let total = to_int(i64::try_from(objects.len()).unwrap_or(i64::MAX));
    objects.truncate(limit);
    Ok(GitObjectFilterPreview {
        nodes: objects,
        total_count: total,
        total_count_younger_than_threshold: younger,
    })
}

/// `PreciseIndex.retentionPolicyOverview`: the retention policies of the index's repository
/// and whether each keeps the index now.
pub async fn retention_policy_overview(
    ctx: &Context<'_>,
    upload: &Upload,
    matches_only: bool,
    query: Option<String>,
    after: Option<String>,
    first: Option<i32>,
) -> Result<CodeIntelligenceRetentionPolicyMatchesConnection> {
    require_reader(ctx)?;
    let svc = services(ctx)?;
    let Some(repo) = load_readable(ctx, i64::from(upload.repo_id.get())).await? else {
        return Err(error(codes::NOT_FOUND, "Repository not found."));
    };
    let git = RepoGit::new(svc.repo_clients.clone(), repo.id, &repo.name);
    let now = Utc::now();
    let retention = Retention::load(&git, &svc.db, now)
        .await
        .map_err(policy_error)?;
    let rows = sourcegull_codeintel_policies::overview(&retention, upload, now)
        .await
        .map_err(policy_error)?;
    let query = query
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| !matches_only || r.matches)
        .filter(|r| match (&query, &r.policy) {
            (None, _) => true,
            (Some(q), Some(p)) => p.name.to_lowercase().contains(q),
            (Some(_), None) => false,
        })
        .collect();
    let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
    let offset = decode_cursor(after.as_deref())?;
    let total = rows.len();
    let mut nodes: Vec<_> = rows
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit + 1).unwrap_or(usize::MAX))
        .map(|r| CodeIntelligenceRetentionPolicyMatch {
            configuration_policy: r.policy.map(PolicyNode),
            matches: r.matches,
            protecting_commits: Some(r.protecting_commits),
        })
        .collect();
    let page_info = PageInfo::new(offset, limit, nodes.len());
    nodes.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(CodeIntelligenceRetentionPolicyMatchesConnection {
        nodes,
        total_count: Some(to_int(i64::try_from(total).unwrap_or(i64::MAX))),
        page_info,
    })
}

/// The fields of `create…`/`update…`, checked.
#[allow(clippy::too_many_arguments)]
fn fields(
    repository_patterns: Option<Vec<String>>,
    name: &str,
    object_type: GitObjectType,
    pattern: &str,
    retention_enabled: bool,
    retention_duration_hours: Option<i32>,
    retain_intermediate_commits: bool,
    indexing_enabled: bool,
    syntactic_indexing_enabled: Option<bool>,
    index_commit_max_age_hours: Option<i32>,
    index_intermediate_commits: bool,
    embeddings_enabled: Option<bool>,
) -> Result<PolicyFields> {
    let checked = check_policy_fields(
        repository_patterns,
        name,
        object_type.to_db(),
        pattern,
        retention_enabled,
        retention_duration_hours,
        retain_intermediate_commits,
        indexing_enabled,
        syntactic_indexing_enabled.unwrap_or(false),
        index_commit_max_age_hours,
        index_intermediate_commits,
    );
    let mut errors = checked.as_ref().err().cloned().unwrap_or_default();
    if embeddings_enabled == Some(true) {
        errors.push("Embeddings are not supported.".into());
    }
    if !errors.is_empty() {
        return Err(validation(errors));
    }
    checked.map_err(validation)
}

/// Policies on all repositories or on patterns that the resources file manages are read-only.
async fn ensure_not_declared(ctx: &Context<'_>, id: i32) -> Result<()> {
    let svc = services(ctx)?;
    let policy = PolicyStore::new(&svc.db).get(id).await.map_err(db_error)?;
    if policy.is_some_and(|p| p.repo_id.is_none()) {
        declared::ensure_not_declared(svc, declared::kinds::CODE_INTEL_POLICIES, &id.to_string())
            .await?;
    }
    Ok(())
}

fn write_error(e: PolicyWriteError) -> async_graphql::Error {
    match e {
        PolicyWriteError::NotFound => error(codes::NOT_FOUND, "Configuration policy not found."),
        PolicyWriteError::DeleteProtected | PolicyWriteError::ChangeProtected => error(
            codes::INVALID_INPUT,
            format!("{}.", capitalize(&e.to_string())),
        ),
        PolicyWriteError::Db(e) => db_error(e),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn policy_id(id: &ID) -> Result<i32> {
    parse_policy_id(id).ok_or_else(|| error(codes::NOT_FOUND, "Configuration policy not found."))
}

#[derive(Default)]
pub struct PoliciesMutation;

#[Object]
impl PoliciesMutation {
    /// Creates a configuration policy (site admins only): for one repository, for the
    /// repositories matching `repositoryPatterns`, or (with neither) for all.
    #[allow(clippy::too_many_arguments)]
    async fn create_code_intelligence_configuration_policy(
        &self,
        ctx: &Context<'_>,
        repository: Option<ID>,
        repository_patterns: Option<Vec<String>>,
        name: String,
        #[graphql(name = "type")] object_type: GitObjectType,
        pattern: String,
        retention_enabled: bool,
        retention_duration_hours: Option<i32>,
        retain_intermediate_commits: bool,
        indexing_enabled: bool,
        syntactic_indexing_enabled: Option<bool>,
        index_commit_max_age_hours: Option<i32>,
        index_intermediate_commits: bool,
        embeddings_enabled: Option<bool>,
    ) -> Result<PolicyNode> {
        require_site_admin(ctx)?;
        if repository.is_some() && repository_patterns.is_some() {
            return Err(validation(vec![
                "Give either a repository or repository patterns, not both.".into(),
            ]));
        }
        let repo: Option<RepoId> = match &repository {
            None => None,
            Some(id) => Some(
                readable_repo(ctx, id, &RepoAccess::All)
                    .await?
                    .ok_or_else(|| error(codes::NOT_FOUND, "Repository not found."))?
                    .id,
            ),
        };
        let f = fields(
            repository_patterns,
            &name,
            object_type,
            &pattern,
            retention_enabled,
            retention_duration_hours,
            retain_intermediate_commits,
            indexing_enabled,
            syntactic_indexing_enabled,
            index_commit_max_age_hours,
            index_intermediate_commits,
            embeddings_enabled,
        )?;
        if repo.is_none() {
            declared::ensure_creatable(services(ctx)?, declared::kinds::CODE_INTEL_POLICIES)
                .await?;
        }
        let p = PolicyStore::new(&services(ctx)?.db)
            .create(repo, &f)
            .await
            .map_err(db_error)?;
        Ok(PolicyNode(p))
    }

    /// Updates a configuration policy (site admins only). Protected policies keep their
    /// name, type, pattern and intermediate commit settings.
    #[allow(clippy::too_many_arguments)]
    async fn update_code_intelligence_configuration_policy(
        &self,
        ctx: &Context<'_>,
        id: ID,
        repository_patterns: Option<Vec<String>>,
        name: String,
        #[graphql(name = "type")] object_type: GitObjectType,
        pattern: String,
        retention_enabled: bool,
        retention_duration_hours: Option<i32>,
        retain_intermediate_commits: bool,
        indexing_enabled: bool,
        syntactic_indexing_enabled: Option<bool>,
        index_commit_max_age_hours: Option<i32>,
        index_intermediate_commits: bool,
        embeddings_enabled: Option<bool>,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let id = policy_id(&id)?;
        ensure_not_declared(ctx, id).await?;
        let f = fields(
            repository_patterns,
            &name,
            object_type,
            &pattern,
            retention_enabled,
            retention_duration_hours,
            retain_intermediate_commits,
            indexing_enabled,
            syntactic_indexing_enabled,
            index_commit_max_age_hours,
            index_intermediate_commits,
            embeddings_enabled,
        )?;
        PolicyStore::new(&services(ctx)?.db)
            .update(id, &f)
            .await
            .map_err(write_error)?;
        Ok(EmptyResponse)
    }

    /// Deletes a configuration policy (site admins only; not protected ones).
    async fn delete_code_intelligence_configuration_policy(
        &self,
        ctx: &Context<'_>,
        policy: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let id = policy_id(&policy)?;
        ensure_not_declared(ctx, id).await?;
        PolicyStore::new(&services(ctx)?.db)
            .delete(id)
            .await
            .map_err(write_error)?;
        Ok(EmptyResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let id = marshal_id(POLICY_KIND, 7);
        assert_eq!(parse_policy_id(&id), Some(7));
        assert_eq!(parse_policy_id(&marshal_id("Repository", 7)), None);
    }

    #[test]
    fn validation() {
        let ok = fields(
            None,
            " Releases ",
            GitObjectType::GitTag,
            "v*",
            true,
            Some(24),
            false,
            false,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(ok.name, "Releases");
        assert_eq!(ok.object_type, DbObjectType::Tag);
        for (patterns, name, t, pattern, hours, embeddings) in [
            (None, "", GitObjectType::GitTag, "v*", None, None),
            (None, "x", GitObjectType::GitBlob, "v*", None, None),
            (None, "x", GitObjectType::GitTree, "[", None, None),
            (None, "x", GitObjectType::GitTree, " ", None, None),
            (Some(vec![]), "x", GitObjectType::GitTree, "*", None, None),
            (
                Some(vec!["{".into()]),
                "x",
                GitObjectType::GitTree,
                "*",
                None,
                None,
            ),
            (None, "x", GitObjectType::GitTree, "*", Some(0), None),
            (None, "x", GitObjectType::GitTree, "*", None, Some(true)),
        ] {
            assert!(
                fields(
                    patterns, name, t, pattern, true, hours, false, false, None, None, false,
                    embeddings
                )
                .is_err()
            );
        }
    }
}
