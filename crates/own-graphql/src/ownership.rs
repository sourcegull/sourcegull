//! Code Ownership: `GitBlob.ownership`, `GitTree.ownership` and `GitCommit.ownership`
//! (ranked owners with reasons), ingested CODEOWNERS files and manual assignments.
//!
//! Types and operations: `OwnershipConnection`, `Ownership`, `Owner = Person | Team`,
//! the `OwnershipReason` union (`CodeownersFileEntry`, `AssignedOwner`,
//! `RecentContributorOwnershipSignal`, `RecentViewOwnershipSignal`), `addCodeownersFile`,
//! `updateCodeownersFile`, `deleteCodeownersFiles`, `codeownersIngestedFiles`,
//! `Repository.ingestedCodeowners`, `assignOwner`, `removeAssignedOwner`, `assignTeam` and
//! `removeAssignedTeam`. `CodeownersFileEntry` also has the matching entry's `section`
//! and `pattern`.
//!
//! Signals and analytics: `RecentContributorOwnershipSignal` (commits in the last 90
//! days, with `contributionCount`) and `RecentViewOwnershipSignal` (file views, with
//! `viewCount`) reasons after the owners, when the signal is enabled for the repository;
//! `ownSignalConfigurations` and `updateOwnSignalConfigurations`; `instanceOwnershipStats` and
//! `GitTree.ownershipStats` (coverage of the default branch, computed in the background), plus
//! `ownershipStatsByRepository` for the admin page.
//!
//! Ingesting CODEOWNERS files is for site admins; assigning owners needs
//! `OWNERSHIP#ASSIGN` and read access to the repository. Owners are computed from the file
//! in force at the viewed commit: an ingested file, else the first found in the code host's
//! locations ([`sourcegull_own::codeowners_paths`]), compiled once per blob OID.
//!
//! The fields that reach ownership from shared types (`GitBlob.ownership`,
//! `GitTree.ownershipStats`, `Repository.ingestedCodeowners`, …) are in the schema assembly
//! and call [`ownership`], [`tree_stats`] and [`ingested_for_repo`]. `Person`, `Repository`
//! and `File2` are returned as references; `Owner` is a union implemented by hand, since its
//! `Person` member is a reference.

use std::borrow::Cow;
use std::sync::Arc;

use async_graphql::parser::types::Field;
use async_graphql::registry::{MetaType, MetaTypeId, Registry};
use async_graphql::{
    Context, ContextSelectionSet, Enum, ID, InputObject, Object, OutputType, Positioned, Result,
    ServerResult, SimpleObject, Union, Value,
};
use chrono::{DateTime, Utc};
use sourcegull_authn::{Actor, Permission};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::git::git_error;
use sourcegull_graphql_core::ids::{parse_repository_id, parse_user_id};
use sourcegull_graphql_core::repo::load_readable;
use sourcegull_graphql_core::shared::{
    ContentLoader, File2Data, File2Ref, GitBlobData, PersonData, PersonRef, RepositoryRef,
    VirtualFileData,
};
use sourcegull_own::codeowners::MAX_CODEOWNERS_BYTES;
use sourcegull_own::ownership::{self, CodeownersOrigin, OwnershipQuery, Reason, ReasonKind};
use sourcegull_own::resolve::{OwnerResolver, ResolvedOwner};
use sourcegull_own::signals::{
    PathStats, RECENT_CONTRIBUTORS, RECENT_VIEWS, SignalConfig, SignalConfigUpdate, SignalStore,
};
use sourcegull_own::store::{Assignee, IngestedCodeowners};
use sourcegull_own::{CompiledCodeowners, codeowners_cache, codeowners_paths};
use sourcegull_services::git::GitReader;
use sourcegull_services::relay::marshal_id;
use sourcegull_types::{Repo, RepoId};

use crate::context::{
    access_error, codes, db_error, error, page_size, require_site_admin, require_viewer,
};
use crate::pagination::{PageInfo, decode_cursor};
use crate::teams::{TeamNode, own_store, parse_team_id};

/// The node kind of ingested CODEOWNERS files.
pub const CODEOWNERS_FILE_KIND: &str = "CodeownersIngestedFile";

const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;

/// The kinds of ownership reasons.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum OwnershipReasonType {
    CodeownersFileEntry,
    AssignedOwner,
    RecentContributorOwnershipSignal,
    RecentViewOwnershipSignal,
}

impl From<ReasonKind> for OwnershipReasonType {
    fn from(k: ReasonKind) -> Self {
        match k {
            ReasonKind::CodeownersFileEntry => OwnershipReasonType::CodeownersFileEntry,
            ReasonKind::AssignedOwner => OwnershipReasonType::AssignedOwner,
            ReasonKind::RecentContributor => OwnershipReasonType::RecentContributorOwnershipSignal,
            ReasonKind::RecentView => OwnershipReasonType::RecentViewOwnershipSignal,
        }
    }
}

/// A file or directory at a commit, whose owners to compute.
pub struct OwnedPath<'a> {
    pub repo: &'a Repo,
    /// The repository's reader.
    pub git: &'a GitReader,
    pub commit_oid: &'a str,
    /// The revision the path was reached by, which the URL of a CODEOWNERS file in the
    /// repository keeps; the commit when unset.
    pub rev: Option<&'a str>,
    /// Path from the repository root (`""` for the root).
    pub path: &'a str,
}

/// The owner is named by an entry of a CODEOWNERS file.
#[derive(SimpleObject, Clone)]
pub struct CodeownersFileEntry {
    pub title: String,
    pub description: String,
    /// The CODEOWNERS file (a `GitBlob`, or a `VirtualFile` for an ingested one).
    pub codeowners_file: File2Ref,
    /// The 1-based line of the matching entry.
    pub rule_line_match: i32,
    /// The entry's section (`codeowners` for entries before any section header).
    pub section: String,
    /// The entry's pattern.
    pub pattern: String,
}

/// The owner was assigned manually.
#[derive(SimpleObject, Clone)]
pub struct AssignedOwner {
    pub title: String,
    pub description: String,
    /// Assigned on this path rather than on a parent directory.
    pub is_direct_match: bool,
}

/// The person authored commits changing the path in the last 90 days.
#[derive(SimpleObject, Clone)]
pub struct RecentContributorOwnershipSignal {
    pub title: String,
    pub description: String,
    /// Commits in the window that changed the path (or something below it).
    pub contribution_count: i32,
}

/// The user viewed the file in the last 90 days.
#[derive(SimpleObject, Clone)]
pub struct RecentViewOwnershipSignal {
    pub title: String,
    pub description: String,
    /// Views in the window.
    pub view_count: i32,
}

/// Why an owner owns a path.
#[allow(clippy::large_enum_variant)]
#[derive(Union, Clone)]
pub enum OwnershipReason {
    CodeownersFileEntry(CodeownersFileEntry),
    RecentContributorOwnershipSignal(RecentContributorOwnershipSignal),
    RecentViewOwnershipSignal(RecentViewOwnershipSignal),
    AssignedOwner(AssignedOwner),
}

/// A person or a team.
#[derive(Clone)]
pub enum Owner {
    Person(PersonRef),
    Team(TeamNode),
}

impl OutputType for Owner {
    fn type_name() -> Cow<'static, str> {
        Cow::Borrowed("Owner")
    }

    fn introspection_type_name(&self) -> Cow<'static, str> {
        match self {
            Owner::Person(_) => PersonRef::type_name(),
            Owner::Team(_) => TeamNode::type_name(),
        }
    }

    fn create_type_info(registry: &mut Registry) -> String {
        registry.create_output_type::<Self, _>(MetaTypeId::Union, |registry| {
            PersonRef::create_type_info(registry);
            TeamNode::create_type_info(registry);
            MetaType::Union {
                name: "Owner".to_owned(),
                description: Some("A person or a team.".to_owned()),
                possible_types: [PersonRef::type_name(), TeamNode::type_name()]
                    .into_iter()
                    .map(Cow::into_owned)
                    .collect(),
                visible: None,
                inaccessible: false,
                tags: Vec::new(),
                rust_typename: Some(std::any::type_name::<Self>()),
                directive_invocations: Vec::new(),
            }
        })
    }

    async fn resolve(
        &self,
        ctx: &ContextSelectionSet<'_>,
        field: &Positioned<Field>,
    ) -> ServerResult<Value> {
        // A member resolves the union's selection set, fragments on the other member
        // included, as the derived unions do.
        match self {
            Owner::Person(p) => p.resolve(ctx, field).await,
            Owner::Team(t) => t.resolve(ctx, field).await,
        }
    }
}

/// An owner and why it owns the path.
#[derive(SimpleObject, Clone)]
#[graphql(name = "Ownership")]
pub struct OwnershipNode {
    pub owner: Owner,
    pub reasons: Vec<OwnershipReason>,
}

/// A page of owners, strongest reason first.
#[derive(SimpleObject)]
pub struct OwnershipConnection {
    pub total_count: i32,
    /// Entries with an owner reason (CODEOWNERS or assignment), as opposed to signals.
    pub total_owners: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<OwnershipNode>,
}

/// The GraphQL form of a resolved owner.
pub fn owner_node(o: &ResolvedOwner) -> Owner {
    match o {
        ResolvedOwner::User(u) => Owner::Person(PersonRef(PersonData {
            name: u.username.clone(),
            email: String::new(),
            display_name: u.display_name.clone().unwrap_or_else(|| u.username.clone()),
            avatar_url: u.avatar_url.clone(),
            user: Some((**u).clone()),
        })),
        ResolvedOwner::Team(t) => Owner::Team(TeamNode::Db(t.clone())),
        ResolvedOwner::ExternalTeam { handle } => Owner::Team(TeamNode::External(handle.clone())),
        ResolvedOwner::Person {
            handle,
            email,
            name,
        } => Owner::Person(PersonRef(PersonData {
            name: handle.clone().or_else(|| name.clone()).unwrap_or_default(),
            email: email.clone().unwrap_or_default(),
            display_name: name
                .clone()
                .or_else(|| handle.clone())
                .or_else(|| email.clone())
                .unwrap_or_default(),
            avatar_url: None,
            user: None,
        })),
    }
}

/// The CODEOWNERS file in force at `base`'s commit: the ingested one, else the first found
/// in the code host's locations in the repository; with the file to link from reasons.
async fn codeowners_at(
    ctx: &Context<'_>,
    base: &OwnedPath<'_>,
) -> Result<Option<(Arc<CompiledCodeowners>, CodeownersOrigin, File2Ref)>> {
    let store = own_store(ctx)?;
    let repo = base.repo;
    if let Some(f) = store.codeowners_for_repo(repo.id).await.map_err(db_error)? {
        let compiled = codeowners_cache().get_text(&f.contents);
        let contents = f.contents;
        let loader: ContentLoader = Arc::new(move || {
            let text = contents.clone();
            Box::pin(async move { Ok(text) })
        });
        let file = File2Ref(File2Data::VirtualFile(VirtualFileData {
            path: "CODEOWNERS".to_owned(),
            url: format!("/site-admin/own/codeowners?repo={}", repo.name),
            loader,
        }));
        return Ok(Some((compiled, CodeownersOrigin::Ingested, file)));
    }
    let git = base.git;
    for &path in codeowners_paths(&repo.external_service_type) {
        let entry = match git.stat(base.commit_oid, path).await {
            Ok(e) => e,
            Err(e) if e.is_not_found() => continue,
            Err(e) => return Err(git_error(e)),
        };
        if entry.kind == sourcegull_rpc::repo::v1::EntryKind::Directory as i32 {
            continue;
        }
        let compiled = match codeowners_cache().get(&entry.oid) {
            Some(c) => c,
            None => {
                let file = git
                    .read_file(base.commit_oid, path, MAX_CODEOWNERS_BYTES as u64)
                    .await
                    .map_err(git_error)?;
                let text = String::from_utf8_lossy(&file.content).into_owned();
                codeowners_cache().get_or_compile(&entry.oid, || text)
            }
        };
        let origin = CodeownersOrigin::Repo {
            path: path.to_owned(),
        };
        let file = File2Ref(File2Data::GitBlob(Box::new(GitBlobData {
            repo: base.repo.clone(),
            commit_oid: base.commit_oid.to_owned(),
            rev: base.rev.map(str::to_owned),
            path: path.to_owned(),
        })));
        return Ok(Some((compiled, origin, file)));
    }
    Ok(None)
}

/// `ownership` of a file or directory.
pub async fn ownership(
    ctx: &Context<'_>,
    base: &OwnedPath<'_>,
    is_dir: bool,
    first: Option<i32>,
    after: Option<String>,
    reasons: Option<Vec<OwnershipReasonType>>,
) -> Result<OwnershipConnection> {
    let db = &crate::context::services(ctx)?.db;
    let store = own_store(ctx)?;
    let resolver = OwnerResolver::new(db);
    let codeowners = codeowners_at(ctx, base).await?;
    let configs = SignalStore::new(db).configs().await.map_err(db_error)?;
    let repo_name = &base.repo.name;
    let list = ownership::compute(
        &store,
        &resolver,
        OwnershipQuery {
            repo: base.repo.id,
            path: base.path,
            is_dir,
            codeowners: codeowners.as_ref().map(|(c, o, _)| (&**c, o.clone())),
            contributors: configs.applies(RECENT_CONTRIBUTORS, repo_name),
            viewers: configs.applies(RECENT_VIEWS, repo_name),
        },
    )
    .await
    .map_err(db_error)?;
    let file = codeowners.map(|(_, _, f)| f);
    let mut nodes: Vec<OwnershipNode> = list
        .into_iter()
        .filter_map(|o| {
            let reasons: Vec<OwnershipReason> = o
                .reasons
                .iter()
                .filter(|r| {
                    reasons
                        .as_ref()
                        .is_none_or(|want| want.contains(&r.kind().into()))
                })
                .filter_map(|r| reason_node(r, file.as_ref()))
                .collect();
            (!reasons.is_empty()).then(|| OwnershipNode {
                owner: owner_node(&o.owner),
                reasons,
            })
        })
        .collect();
    let total = nodes.len();
    let total_owners = nodes
        .iter()
        .filter(|n| {
            n.reasons.iter().any(|r| {
                matches!(
                    r,
                    OwnershipReason::CodeownersFileEntry(_) | OwnershipReason::AssignedOwner(_)
                )
            })
        })
        .count();
    let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
    let offset = decode_cursor(after.as_deref())?;
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let page: Vec<_> = nodes
        .drain(start..)
        .take(usize::try_from(limit).unwrap_or(0) + 1)
        .collect();
    let page_info = PageInfo::new(offset, limit, page.len());
    Ok(OwnershipConnection {
        total_count: i32::try_from(total).unwrap_or(i32::MAX),
        total_owners: i32::try_from(total_owners).unwrap_or(i32::MAX),
        page_info,
        nodes: page
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .collect(),
    })
}

fn reason_node(r: &Reason, file: Option<&File2Ref>) -> Option<OwnershipReason> {
    Some(match r {
        Reason::CodeownersFileEntry {
            line,
            section,
            pattern,
            ..
        } => OwnershipReason::CodeownersFileEntry(CodeownersFileEntry {
            title: "codeowners".to_owned(),
            description: "Owner is associated with a rule in a CODEOWNERS file.".to_owned(),
            codeowners_file: file?.clone(),
            rule_line_match: i32::try_from(*line).unwrap_or(i32::MAX),
            section: section.clone(),
            pattern: pattern.clone(),
        }),
        Reason::AssignedOwner { direct, .. } => OwnershipReason::AssignedOwner(AssignedOwner {
            title: "assigned owner".to_owned(),
            description: if *direct {
                "Owner is manually assigned.".to_owned()
            } else {
                "Owner is manually assigned to a parent directory.".to_owned()
            },
            is_direct_match: *direct,
        }),
        Reason::RecentContributor { count } => {
            OwnershipReason::RecentContributorOwnershipSignal(RecentContributorOwnershipSignal {
                title: "recent contributor".to_owned(),
                description: "Associated because they have contributed to this file in the \
                              last 90 days."
                    .to_owned(),
                contribution_count: i32::try_from(*count).unwrap_or(i32::MAX),
            })
        }
        Reason::RecentView { count } => {
            OwnershipReason::RecentViewOwnershipSignal(RecentViewOwnershipSignal {
                title: "recent view".to_owned(),
                description: "Associated because they have viewed this file in the last 90 \
                              days."
                    .to_owned(),
                view_count: i32::try_from(*count).unwrap_or(i32::MAX),
            })
        }
    })
}

// ------------------------------------------------------------------ signals and analytics

/// The configuration of an ownership signal.
#[derive(SimpleObject, Clone)]
pub struct OwnSignalConfiguration {
    /// `recent-contributors`, `recent-views` or `analytics`.
    pub name: String,
    pub description: String,
    pub is_enabled: bool,
    /// Glob patterns over repository names (`*` matches any characters) the signal skips.
    pub excluded_repo_patterns: Vec<String>,
}

impl From<SignalConfig> for OwnSignalConfiguration {
    fn from(c: SignalConfig) -> Self {
        OwnSignalConfiguration {
            name: c.name,
            description: c.description,
            is_enabled: c.enabled,
            excluded_repo_patterns: c.excluded_repo_patterns,
        }
    }
}

/// A change to one signal configuration.
#[derive(InputObject)]
pub struct OwnSignalConfigurationUpdate {
    pub name: String,
    /// Unset keeps the current patterns.
    pub excluded_repo_patterns: Option<Vec<String>>,
    pub enabled: bool,
}

/// The input of `updateOwnSignalConfigurations`.
#[derive(InputObject)]
pub struct UpdateSignalConfigurationsInput {
    pub configs: Vec<OwnSignalConfigurationUpdate>,
}

/// Ownership coverage of files (the default branch), computed in the background.
#[derive(SimpleObject, Clone, Default)]
pub struct OwnershipStats {
    pub total_files: i32,
    /// Files with an owner from a CODEOWNERS file.
    pub total_codeowned_files: i32,
    /// Files with any owner (CODEOWNERS or assigned).
    pub total_owned_files: i32,
    /// Files with an assigned owner.
    pub total_assigned_ownership_files: i32,
    /// When the numbers were computed; null before the first run.
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<PathStats> for OwnershipStats {
    fn from(s: PathStats) -> Self {
        let int = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
        OwnershipStats {
            total_files: int(s.total_files),
            total_codeowned_files: int(s.codeowned_files),
            total_owned_files: int(s.owned_files),
            total_assigned_ownership_files: int(s.assigned_files),
            updated_at: s.updated_at,
        }
    }
}

/// The ownership coverage of one repository.
#[derive(SimpleObject, Clone)]
pub struct RepositoryOwnershipStats {
    /// The repository (null when the viewer may not read it).
    pub repository: Option<RepositoryRef>,
    pub name: String,
    pub stats: OwnershipStats,
}

/// A page of repositories' coverage, by name.
#[derive(SimpleObject)]
pub struct RepositoryOwnershipStatsConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<RepositoryOwnershipStats>,
}

/// `GitTree.ownershipStats`: the coverage of the directory on the default branch.
pub async fn tree_stats(ctx: &Context<'_>, repo: RepoId, path: &str) -> Result<OwnershipStats> {
    let db = &crate::context::services(ctx)?.db;
    let path = sourcegull_own::store::normalize_path(path);
    Ok(SignalStore::new(db)
        .path_stats(repo, &path)
        .await
        .map_err(db_error)?
        .map(OwnershipStats::from)
        .unwrap_or_default())
}

// ------------------------------------------------------------------ ingested files

/// A CODEOWNERS file ingested for a repository.
#[derive(Clone)]
pub struct CodeownersIngestedFileNode(pub IngestedCodeowners);

#[Object(name = "CodeownersIngestedFile")]
impl CodeownersIngestedFileNode {
    pub async fn id(&self) -> ID {
        ID(marshal_id(CODEOWNERS_FILE_KIND, self.0.id))
    }

    /// The file's text.
    async fn contents(&self) -> &str {
        &self.0.contents
    }

    /// The repository (null when the viewer may not read it).
    async fn repository(&self, ctx: &Context<'_>) -> Result<Option<RepositoryRef>> {
        Ok(load_readable(ctx, i64::from(self.0.repo_id.get()))
            .await?
            .map(RepositoryRef))
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

/// A page of ingested CODEOWNERS files.
#[derive(SimpleObject)]
pub struct CodeownersIngestedFileConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<CodeownersIngestedFileNode>,
}

/// The input of `addCodeownersFile` and `updateCodeownersFile`: the text and the repository
/// (by ID or by name).
#[derive(InputObject)]
pub struct CodeownersFileInput {
    pub file_contents: String,
    #[graphql(name = "repoID")]
    pub repo_id: Option<ID>,
    pub repo_name: Option<String>,
}

/// A repository whose ingested file to delete (by ID or by name).
#[derive(InputObject)]
pub struct DeleteCodeownersFilesInput {
    #[graphql(name = "repoID")]
    pub repo_id: Option<ID>,
    pub repo_name: Option<String>,
}

/// Who to assign to what.
#[derive(InputObject)]
pub struct AssignOwnerOrTeamInput {
    /// A `User` ID (`assignOwner`) or a `Team` ID (`assignTeam`).
    #[graphql(name = "assignedOwnerID")]
    pub assigned_owner_id: ID,
    #[graphql(name = "repoID")]
    pub repo_id: ID,
    /// A file or directory path, or `""` for the whole repository.
    pub absolute_path: String,
}

/// The repository named by an ID or a name, as the viewer may read it.
async fn find_repo(ctx: &Context<'_>, id: Option<&ID>, name: Option<&str>) -> Result<RepoId> {
    let repo = match (id, name) {
        (Some(id), None) => {
            let id = parse_repository_id(id)?;
            load_readable(ctx, i64::from(id.get())).await?
        }
        (None, Some(name)) => {
            let access = crate::context::repo_access(ctx).await?;
            crate::context::services(ctx)?
                .db
                .repos()
                .get_by_name_authorized(name, &access)
                .await
                .map_err(db_error)?
        }
        _ => {
            return Err(error(
                codes::INVALID_INPUT,
                "Specify the repository by ID or by name, not both.",
            ));
        }
    };
    repo.map(|r| r.id)
        .ok_or_else(|| error(codes::NOT_FOUND, "Repository not found."))
}

fn require_assign<'a>(ctx: &Context<'a>) -> Result<&'a Actor> {
    let actor = require_viewer(ctx)?;
    sourcegull_authn::access::require_permission(actor, Permission::OwnershipAssign)
        .map_err(access_error)?;
    Ok(actor)
}

/// `Repository.ingestedCodeowners`.
pub async fn ingested_for_repo(
    ctx: &Context<'_>,
    repo: RepoId,
) -> Result<Option<CodeownersIngestedFileNode>> {
    Ok(own_store(ctx)?
        .codeowners_for_repo(repo)
        .await
        .map_err(db_error)?
        .map(CodeownersIngestedFileNode))
}

/// Loads an ingested file for `node(id:)` (site admins only).
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<CodeownersIngestedFileNode>> {
    require_site_admin(ctx)?;
    Ok(own_store(ctx)?
        .get_codeowners(id)
        .await
        .map_err(db_error)?
        .map(CodeownersIngestedFileNode))
}

#[derive(Default)]
pub struct OwnQuery;

#[Object]
impl OwnQuery {
    /// The ownership signals' configurations (site admins only).
    async fn own_signal_configurations(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<OwnSignalConfiguration>> {
        require_site_admin(ctx)?;
        let db = &crate::context::services(ctx)?.db;
        let configs = SignalStore::new(db).configs().await.map_err(db_error)?;
        Ok(configs.0.into_iter().map(Into::into).collect())
    }

    /// Ownership coverage of all repositories' default branches (site admins only).
    async fn instance_ownership_stats(&self, ctx: &Context<'_>) -> Result<OwnershipStats> {
        require_site_admin(ctx)?;
        let db = &crate::context::services(ctx)?.db;
        Ok(SignalStore::new(db)
            .instance_stats()
            .await
            .map_err(db_error)?
            .into())
    }

    /// Ownership coverage per repository, by name; `after` is the last name seen (site
    /// admins only).
    async fn ownership_stats_by_repository(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<RepositoryOwnershipStatsConnection> {
        require_site_admin(ctx)?;
        let db = &crate::context::services(ctx)?.db;
        let store = SignalStore::new(db);
        let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
        let mut rows = store
            .repo_stats(limit + 1, after.as_deref())
            .await
            .map_err(db_error)?;
        let total = store.count_repo_stats().await.map_err(db_error)?;
        let has_next_page = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        let mut nodes = Vec::with_capacity(rows.len());
        for (id, name, stats) in rows {
            nodes.push(RepositoryOwnershipStats {
                repository: load_readable(ctx, i64::from(id.get()))
                    .await?
                    .map(RepositoryRef),
                name,
                stats: stats.into(),
            });
        }
        Ok(RepositoryOwnershipStatsConnection {
            total_count: crate::context::to_int(total),
            page_info: PageInfo {
                has_next_page,
                end_cursor: has_next_page
                    .then(|| nodes.last().map(|n| n.name.clone()))
                    .flatten(),
            },
            nodes,
        })
    }

    /// Ingested CODEOWNERS files by ID (site admins only); `after` is the last ID seen.
    async fn codeowners_ingested_files(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<i32>,
    ) -> Result<CodeownersIngestedFileConnection> {
        require_site_admin(ctx)?;
        let store = own_store(ctx)?;
        let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
        let mut files = store
            .list_codeowners(limit + 1, after.map(i64::from))
            .await
            .map_err(db_error)?;
        let total = store.count_codeowners().await.map_err(db_error)?;
        let has_next_page = i64::try_from(files.len()).unwrap_or(i64::MAX) > limit;
        files.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(CodeownersIngestedFileConnection {
            total_count: crate::context::to_int(total),
            page_info: PageInfo {
                has_next_page,
                end_cursor: has_next_page
                    .then(|| files.last().map(|f| f.id.to_string()))
                    .flatten(),
            },
            nodes: files.into_iter().map(CodeownersIngestedFileNode).collect(),
        })
    }
}

#[derive(Default)]
pub struct OwnMutation;

#[Object]
impl OwnMutation {
    /// Enables or disables ownership signals and sets the repositories they skip (site
    /// admins only). Returns all configurations.
    async fn update_own_signal_configurations(
        &self,
        ctx: &Context<'_>,
        input: UpdateSignalConfigurationsInput,
    ) -> Result<Vec<OwnSignalConfiguration>> {
        require_site_admin(ctx)?;
        let db = &crate::context::services(ctx)?.db;
        let updates: Vec<SignalConfigUpdate> = input
            .configs
            .into_iter()
            .map(|c| SignalConfigUpdate {
                name: c.name,
                enabled: c.enabled,
                excluded_repo_patterns: c.excluded_repo_patterns.map(|ps| {
                    ps.into_iter()
                        .map(|p| p.trim().to_owned())
                        .filter(|p| !p.is_empty())
                        .collect()
                }),
            })
            .collect();
        let configs = SignalStore::new(db)
            .update_configs(&updates)
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::Invalid(msg) => error(codes::INVALID_INPUT, msg),
                other => db_error(other),
            })?;
        Ok(configs.0.into_iter().map(Into::into).collect())
    }

    /// Ingests a CODEOWNERS file for a repository; it replaces the repository's own file
    /// (site admins only).
    async fn add_codeowners_file(
        &self,
        ctx: &Context<'_>,
        input: CodeownersFileInput,
    ) -> Result<CodeownersIngestedFileNode> {
        require_site_admin(ctx)?;
        let repo = find_repo(ctx, input.repo_id.as_ref(), input.repo_name.as_deref()).await?;
        check_size(&input.file_contents)?;
        own_store(ctx)?
            .add_codeowners(repo, &input.file_contents)
            .await
            .map(CodeownersIngestedFileNode)
            .map_err(|e| match e {
                sourcegull_db::DbError::AlreadyExists { .. } => error(
                    codes::ALREADY_EXISTS,
                    "A CODEOWNERS file has already been ingested for this repository.",
                ),
                other => db_error(other),
            })
    }

    /// Replaces a repository's ingested CODEOWNERS file (site admins only).
    async fn update_codeowners_file(
        &self,
        ctx: &Context<'_>,
        input: CodeownersFileInput,
    ) -> Result<CodeownersIngestedFileNode> {
        require_site_admin(ctx)?;
        let repo = find_repo(ctx, input.repo_id.as_ref(), input.repo_name.as_deref()).await?;
        check_size(&input.file_contents)?;
        own_store(ctx)?
            .update_codeowners(repo, &input.file_contents)
            .await
            .map(CodeownersIngestedFileNode)
            .map_err(|e| match e {
                sourcegull_db::DbError::NotFound(_) => error(
                    codes::NOT_FOUND,
                    "No CODEOWNERS file has been ingested for this repository.",
                ),
                other => db_error(other),
            })
    }

    /// Deletes the ingested CODEOWNERS files of repositories (site admins only).
    async fn delete_codeowners_files(
        &self,
        ctx: &Context<'_>,
        repositories: Vec<DeleteCodeownersFilesInput>,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let mut repos = Vec::with_capacity(repositories.len());
        for r in &repositories {
            repos.push(find_repo(ctx, r.repo_id.as_ref(), r.repo_name.as_deref()).await?);
        }
        own_store(ctx)?
            .delete_codeowners(&repos)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Assigns a user as owner of a repository, directory or file (`OWNERSHIP#ASSIGN`).
    async fn assign_owner(
        &self,
        ctx: &Context<'_>,
        input: AssignOwnerOrTeamInput,
    ) -> Result<EmptyResponse> {
        let (actor, assignee, repo) = assignment(ctx, &input, false).await?;
        own_store(ctx)?
            .assign(assignee, repo, &input.absolute_path, Some(actor.user_id))
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Removes an assigned owner (`OWNERSHIP#ASSIGN`).
    async fn remove_assigned_owner(
        &self,
        ctx: &Context<'_>,
        input: AssignOwnerOrTeamInput,
    ) -> Result<EmptyResponse> {
        let (_, assignee, repo) = assignment(ctx, &input, false).await?;
        own_store(ctx)?
            .remove_assignment(assignee, repo, &input.absolute_path)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Assigns a team as owner of a repository, directory or file (`OWNERSHIP#ASSIGN`).
    async fn assign_team(
        &self,
        ctx: &Context<'_>,
        input: AssignOwnerOrTeamInput,
    ) -> Result<EmptyResponse> {
        let (actor, assignee, repo) = assignment(ctx, &input, true).await?;
        own_store(ctx)?
            .assign(assignee, repo, &input.absolute_path, Some(actor.user_id))
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Removes an assigned team (`OWNERSHIP#ASSIGN`).
    async fn remove_assigned_team(
        &self,
        ctx: &Context<'_>,
        input: AssignOwnerOrTeamInput,
    ) -> Result<EmptyResponse> {
        let (_, assignee, repo) = assignment(ctx, &input, true).await?;
        own_store(ctx)?
            .remove_assignment(assignee, repo, &input.absolute_path)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }
}

fn check_size(text: &str) -> Result<()> {
    if text.len() > MAX_CODEOWNERS_BYTES {
        return Err(error(
            codes::TOO_LARGE,
            "CODEOWNERS files are limited to 10 MB.",
        ));
    }
    Ok(())
}

/// Checks the permission and resolves the assignee and the readable repository.
async fn assignment<'a>(
    ctx: &Context<'a>,
    input: &AssignOwnerOrTeamInput,
    team: bool,
) -> Result<(&'a Actor, Assignee, RepoId)> {
    let actor = require_assign(ctx)?;
    let repo = find_repo(ctx, Some(&input.repo_id), None).await?;
    let db = &crate::context::services(ctx)?.db;
    let assignee = if team {
        let id = parse_team_id(&input.assigned_owner_id)?;
        own_store(ctx)?
            .get_team(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Team not found."))?;
        Assignee::Team(id)
    } else {
        let id = parse_user_id(&input.assigned_owner_id)?;
        db.users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "User not found."))?;
        Assignee::User(id)
    };
    Ok((actor, assignee, repo))
}
