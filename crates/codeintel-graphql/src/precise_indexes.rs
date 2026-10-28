//! Precise code intelligence indexes over GraphQL: `preciseIndexes`,
//! `indexerKeys` and the delete and reindex mutations.
//!
//! A `PreciseIndex` is a SCIP upload, an auto-indexing job, or both (the upload a job
//! produced stands for the job). IDs are `PreciseIndex:"U:<upload>"`,
//! `"I:<job>"` or `"U:<upload>:I:<job>"`. Listings merge uploads and jobs that have no upload
//! yet, newest first.
//!
//! Anyone who can read a repository sees its indexes; deleting and reindexing need a site
//! admin. Reindexing an index an auto-indexing job made (or a job) marks it `shouldReindex`
//! and queues the job again; an upload from elsewhere is processed again from its stored
//! index.

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_codeintel_scip::store::{
    CodeIntelStore, IndexJobRow, IndexJobStore, ListIndexJobs, ListUploads, Upload, UploadState,
};
use sourcegull_services::relay::{marshal_id, marshal_string_id, unmarshal_id};
use sourcegull_types::{RepoAccess, UserId};

use crate::autoindex::IndexSteps;
use crate::context::{
    codes, db_error, error, page_size, repo_access, require_site_admin, services, to_int,
};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::ids::{REPOSITORY_KIND, parse_repository_id};
use sourcegull_graphql_core::repo::{load_readable, require_reader};
use sourcegull_types::Repo;

/// Node kind of precise index IDs.
pub const PRECISE_INDEX_KIND: &str = "PreciseIndex";
const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;

/// `PreciseIndex.state`. The indexing states are those of auto-indexing jobs.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum PreciseIndexState {
    UploadingIndex,
    QueuedForProcessing,
    Processing,
    ProcessingErrored,
    Completed,
    Deleting,
    Deleted,
    QueuedForIndexing,
    Indexing,
    IndexingErrored,
    IndexingCompleted,
}

impl PreciseIndexState {
    fn of(state: UploadState) -> Self {
        match state {
            UploadState::Uploading => Self::UploadingIndex,
            UploadState::Queued => Self::QueuedForProcessing,
            UploadState::Processing => Self::Processing,
            UploadState::Errored | UploadState::Failed | UploadState::Canceled => {
                Self::ProcessingErrored
            }
            UploadState::Completed => Self::Completed,
            UploadState::Deleting => Self::Deleting,
            UploadState::Deleted => Self::Deleted,
        }
    }

    /// The state shown for an index job (one without an upload).
    fn of_job(state: &str) -> Self {
        match state {
            "queued" => Self::QueuedForIndexing,
            "processing" => Self::Indexing,
            "completed" => Self::IndexingCompleted,
            _ => Self::IndexingErrored,
        }
    }

    /// The index job states shown as `self` (none for upload states).
    fn job_states(self) -> &'static [&'static str] {
        match self {
            Self::QueuedForIndexing => &["queued"],
            Self::Indexing => &["processing"],
            Self::IndexingErrored => &["errored", "failed", "canceled"],
            Self::IndexingCompleted => &["completed"],
            _ => &[],
        }
    }

    /// The upload states shown as `self` (none for indexing states).
    fn upload_states(self) -> &'static [UploadState] {
        match self {
            Self::UploadingIndex => &[UploadState::Uploading],
            Self::QueuedForProcessing => &[UploadState::Queued],
            Self::Processing => &[UploadState::Processing],
            Self::ProcessingErrored => &[
                UploadState::Errored,
                UploadState::Failed,
                UploadState::Canceled,
            ],
            Self::Completed => &[UploadState::Completed],
            Self::Deleting => &[UploadState::Deleting],
            Self::Deleted => &[UploadState::Deleted],
            Self::QueuedForIndexing
            | Self::Indexing
            | Self::IndexingErrored
            | Self::IndexingCompleted => &[],
        }
    }
}

/// A known SCIP indexer: its key (language ecosystem), the names it uploads under, its source
/// and image.
struct KnownIndexer {
    key: &'static str,
    name: &'static str,
    aliases: &'static [&'static str],
    url: &'static str,
    image: &'static str,
}

const KNOWN_INDEXERS: &[KnownIndexer] = &[
    KnownIndexer {
        key: "go",
        name: "scip-go",
        aliases: &["lsif-go"],
        url: "https://github.com/sourcegraph/scip-go",
        image: "sourcegraph/scip-go",
    },
    KnownIndexer {
        key: "typescript",
        name: "scip-typescript",
        aliases: &["lsif-tsc", "lsif-node"],
        url: "https://github.com/sourcegraph/scip-typescript",
        image: "sourcegraph/scip-typescript",
    },
    KnownIndexer {
        key: "java",
        name: "scip-java",
        aliases: &["lsif-java"],
        url: "https://github.com/sourcegraph/scip-java",
        image: "sourcegraph/scip-java",
    },
    KnownIndexer {
        key: "python",
        name: "scip-python",
        aliases: &[],
        url: "https://github.com/sourcegraph/scip-python",
        image: "sourcegraph/scip-python",
    },
    KnownIndexer {
        key: "ruby",
        name: "scip-ruby",
        aliases: &[],
        url: "https://github.com/sourcegraph/scip-ruby",
        image: "sourcegraph/scip-ruby",
    },
    KnownIndexer {
        key: "rust",
        name: "rust-analyzer",
        aliases: &[],
        url: "https://github.com/rust-lang/rust-analyzer",
        image: "sourcegraph/scip-rust",
    },
    KnownIndexer {
        key: "dotnet",
        name: "scip-dotnet",
        aliases: &[],
        url: "https://github.com/sourcegraph/scip-dotnet",
        image: "sourcegraph/scip-dotnet",
    },
    KnownIndexer {
        key: "clang",
        name: "scip-clang",
        aliases: &[],
        url: "https://github.com/sourcegraph/scip-clang",
        image: "sourcegraph/scip-clang",
    },
];

/// The bare tool name of an indexer as uploads name it: `sourcegraph/scip-go:v0.1` →
/// `scip-go`.
fn bare_name(indexer: &str) -> &str {
    let name = indexer.rsplit('/').next().unwrap_or(indexer);
    name.split([':', '@']).next().unwrap_or(name)
}

fn known_indexer(indexer: &str) -> Option<&'static KnownIndexer> {
    let name = bare_name(indexer);
    KNOWN_INDEXERS
        .iter()
        .find(|k| k.name == name || k.aliases.contains(&name))
}

/// The indexer key of an upload's indexer (the bare name for unknown indexers).
fn indexer_key(indexer: &str) -> &str {
    known_indexer(indexer).map_or_else(|| bare_name(indexer), |k| k.key)
}

/// `CodeIntelIndexer`: the tool that made an index.
#[derive(SimpleObject, Debug, Clone)]
pub struct CodeIntelIndexer {
    /// The language ecosystem the indexer supports (the `indexerKey` filter).
    key: String,
    name: String,
    /// The indexer's source, if known.
    url: String,
    /// The Docker image auto-indexing runs, if known.
    image_name: Option<String>,
}

impl CodeIntelIndexer {
    pub(crate) fn of(indexer: &str) -> Option<Self> {
        if indexer.is_empty() {
            return None;
        }
        Some(match known_indexer(indexer) {
            Some(k) => Self {
                key: k.key.into(),
                name: k.name.into(),
                url: k.url.into(),
                // An index job names the exact image it runs.
                image_name: Some(if indexer.contains(['/', '@']) {
                    indexer.into()
                } else {
                    k.image.into()
                }),
            },
            None => Self {
                key: bare_name(indexer).into(),
                name: indexer.into(),
                url: String::new(),
                // An image (`registry/name:tag` or a digest) is what auto-indexing ran.
                image_name: indexer.contains(['/', '@', ':']).then(|| indexer.into()),
            },
        })
    }
}

/// `CodeIntelRepository`.
pub struct CodeIntelRepository(pub(crate) Repo);

#[Object]
impl CodeIntelRepository {
    async fn id(&self) -> ID {
        ID(marshal_id(REPOSITORY_KIND, i64::from(self.0.id.get())))
    }

    async fn name(&self) -> &str {
        &self.0.name
    }

    async fn url(&self) -> String {
        format!("/{}", self.0.name)
    }
}

/// `CodeIntelCommit`.
pub struct CodeIntelCommit {
    repo: Repo,
    oid: String,
}

#[Object]
impl CodeIntelCommit {
    async fn id(&self) -> ID {
        ID(marshal_string_id(
            "GitCommit",
            &format!("{}@{}", self.repo.id.get(), self.oid),
        ))
    }

    async fn oid(&self) -> &str {
        &self.oid
    }

    #[graphql(name = "abbreviatedOID")]
    async fn abbreviated_oid(&self) -> &str {
        &self.oid[..self.oid.len().min(7)]
    }

    async fn url(&self) -> String {
        format!("/{}/-/commit/{}", self.repo.name, self.oid)
    }

    async fn repository(&self) -> CodeIntelRepository {
        CodeIntelRepository(self.repo.clone())
    }
}

/// `CodeIntelGitTree`: the directory an index covers.
pub struct CodeIntelGitTree {
    repo: Repo,
    commit: String,
    /// `""` or `dir/`.
    root: String,
}

#[Object]
impl CodeIntelGitTree {
    /// The directory, relative to the repository root, without a trailing slash.
    async fn path(&self) -> &str {
        self.root.trim_end_matches('/')
    }

    async fn name(&self) -> &str {
        let path = self.root.trim_end_matches('/');
        path.rsplit('/').next().unwrap_or(path)
    }

    async fn commit(&self) -> CodeIntelCommit {
        CodeIntelCommit {
            repo: self.repo.clone(),
            oid: self.commit.clone(),
        }
    }

    async fn url(&self) -> String {
        let base = format!("/{}@{}", self.repo.name, self.commit);
        match self.root.trim_end_matches('/') {
            "" => base,
            path => format!("{base}/-/tree/{path}"),
        }
    }

    async fn repository(&self) -> CodeIntelRepository {
        CodeIntelRepository(self.repo.clone())
    }
}

/// Extension: the GitLab CI job of an upload made with a CI job token.
#[derive(SimpleObject, Debug, Clone)]
pub struct PreciseIndexCiJob {
    /// The GitLab job id.
    pub job_id: String,
    /// The branch or tag the job ran for.
    #[graphql(name = "ref")]
    pub git_ref: String,
    /// The GitLab username of the user who triggered the job (empty when unknown).
    pub username: String,
}

/// `PreciseIndex`: a SCIP index of one directory of a repository at a commit: an upload,
/// the auto-indexing job that is making one, or both.
#[derive(Debug, Clone)]
pub struct PreciseIndexNode {
    pub upload: Option<Upload>,
    pub job: Option<IndexJobRow>,
}

impl PreciseIndexNode {
    fn repo_commit_root(&self) -> (sourcegull_types::RepoId, &str, &str) {
        match (&self.upload, &self.job) {
            (Some(u), _) => (u.repo_id, &u.commit, &u.root),
            (None, Some(j)) => (j.job.repo_id, &j.job.commit, &j.job.root),
            (None, None) => unreachable!("a precise index has an upload or a job"),
        }
    }
}

#[Object(name = "PreciseIndex")]
impl PreciseIndexNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        let mut parts = Vec::new();
        if let Some(u) = &self.upload {
            parts.push(format!("U:{}", u.id));
        }
        if let Some(j) = &self.job {
            parts.push(format!("I:{}", j.id));
        }
        ID(marshal_string_id(PRECISE_INDEX_KIND, &parts.join(":")))
    }

    /// The directory the index covers; null when the repository is gone or not visible.
    async fn project_root(&self, ctx: &Context<'_>) -> Result<Option<CodeIntelGitTree>> {
        let (repo, commit, root) = self.repo_commit_root();
        let repo = load_readable(ctx, i64::from(repo.get())).await?;
        Ok(repo.map(|repo| CodeIntelGitTree {
            repo,
            commit: commit.to_owned(),
            root: root.to_owned(),
        }))
    }

    async fn input_commit(&self) -> &str {
        self.repo_commit_root().1
    }

    async fn input_root(&self) -> &str {
        self.repo_commit_root().2
    }

    async fn input_indexer(&self) -> &str {
        match (&self.upload, &self.job) {
            (Some(u), _) => &u.indexer,
            (None, Some(j)) => &j.job.indexer,
            (None, None) => "",
        }
    }

    /// The indexer; for auto-indexing jobs, with the image that ran.
    async fn indexer(&self) -> Option<CodeIntelIndexer> {
        match (&self.upload, &self.job) {
            (_, Some(j)) => CodeIntelIndexer::of(&j.job.indexer),
            (Some(u), None) => CodeIntelIndexer::of(&u.indexer),
            (None, None) => None,
        }
    }

    async fn state(&self) -> PreciseIndexState {
        match (&self.upload, &self.job) {
            (Some(u), _) => PreciseIndexState::of(u.state),
            (None, Some(j)) => PreciseIndexState::of_job(&j.state),
            (None, None) => PreciseIndexState::Deleted,
        }
    }

    /// When the index job was queued (null for uploaded indexes).
    async fn queued_at(&self) -> Option<DateTime<Utc>> {
        self.job.as_ref().map(|j| j.queued_at)
    }

    async fn indexing_started_at(&self) -> Option<DateTime<Utc>> {
        self.job.as_ref().and_then(|j| j.started_at)
    }

    async fn indexing_finished_at(&self) -> Option<DateTime<Utc>> {
        self.job.as_ref().and_then(|j| j.finished_at)
    }

    /// When the index was uploaded (null until an index job uploads one).
    async fn uploaded_at(&self) -> Option<DateTime<Utc>> {
        self.upload.as_ref().map(|u| u.uploaded_at)
    }

    async fn processing_started_at(&self) -> Option<DateTime<Utc>> {
        self.upload.as_ref().and_then(|u| u.started_at)
    }

    async fn processing_finished_at(&self) -> Option<DateTime<Utc>> {
        self.upload.as_ref().and_then(|u| u.finished_at)
    }

    /// The indexing or processing error. An index that will be retried keeps its last error
    /// here.
    async fn failure(&self) -> Option<&str> {
        self.upload
            .as_ref()
            .and_then(|u| u.failure_message.as_deref())
            .or_else(|| self.job.as_ref().and_then(|j| j.failure_message.as_deref()))
    }

    /// The position in the indexing or processing queue (1 = next).
    async fn place_in_queue(&self, ctx: &Context<'_>) -> Result<Option<i32>> {
        let svc = services(ctx)?;
        if let Some(j) = self.job.as_ref().filter(|j| j.state == "queued") {
            let n = IndexJobStore::new(&svc.db)
                .place_in_queue(j.id)
                .await
                .map_err(db_error)?;
            return Ok(n.map(to_int));
        }
        let Some(u) = self
            .upload
            .as_ref()
            .filter(|u| u.state == UploadState::Queued)
        else {
            return Ok(None);
        };
        let n = CodeIntelStore::new(&svc.db)
            .place_in_queue(u.id)
            .await
            .map_err(db_error)?;
        Ok(n.map(to_int))
    }

    /// The auto-indexing job's configuration and execution logs (null for uploads from
    /// elsewhere).
    async fn steps(&self) -> Option<IndexSteps> {
        self.job
            .as_ref()
            .map(|j| IndexSteps::of_job(&j.job, &j.execution_logs))
    }

    /// Whether the index is marked to be replaced by a new auto-indexing job.
    async fn should_reindex(&self) -> bool {
        match (&self.upload, &self.job) {
            (Some(u), j) => u.should_reindex && j.as_ref().is_none_or(|j| j.job.should_reindex),
            (None, Some(j)) => j.job.should_reindex,
            (None, None) => false,
        }
    }

    /// Extension: the GitLab CI job that uploaded the index with its job token.
    async fn gitlab_ci_job(&self) -> Option<PreciseIndexCiJob> {
        let job = self.upload.as_ref()?.ci_job.as_ref()?;
        Some(PreciseIndexCiJob {
            job_id: job.job_id.to_string(),
            git_ref: job.git_ref.clone(),
            username: job.username.clone(),
        })
    }

    /// Extension: bytes uploaded (as sent, usually gzip-compressed).
    async fn upload_size(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        self.upload
            .as_ref()
            .and_then(|u| u.upload_size)
            .map(|n| n as f64)
    }

    /// Extension: documents, occurrences and symbols in the processed index.
    async fn num_documents(&self) -> Option<i32> {
        self.upload.as_ref().and_then(|u| u.num_documents)
    }

    async fn num_occurrences(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        self.upload
            .as_ref()
            .and_then(|u| u.num_occurrences)
            .map(|n| n as f64)
    }

    async fn num_symbols(&self) -> Option<i32> {
        self.upload.as_ref().and_then(|u| u.num_symbols)
    }

    /// Whether the index serves precise navigation at the head of the repository's default
    /// branch (it is the nearest one there for its root and indexer).
    async fn is_latest_for_repo(&self, ctx: &Context<'_>) -> Result<bool> {
        let Some(u) = self
            .upload
            .as_ref()
            .filter(|u| u.state == UploadState::Completed)
        else {
            return Ok(false);
        };
        let Some(repo) = load_readable(ctx, i64::from(u.repo_id.get())).await? else {
            return Ok(false);
        };
        match crate::codenav::code_nav(ctx)?
            .is_latest_for_repo(&repo, u.id)
            .await
        {
            Ok(latest) => Ok(latest),
            Err(e) => {
                tracing::debug!(upload = u.id, error = %e, "checking isLatestForRepo");
                Ok(false)
            }
        }
    }

    /// The retention policies of the index's repository and whether each keeps the index
    /// now; the tip of the default branch (no policy) is listed first when it keeps it.
    /// Empty until there is an upload.
    async fn retention_policy_overview(
        &self,
        ctx: &Context<'_>,
        matches_only: bool,
        query: Option<String>,
        after: Option<String>,
        first: Option<i32>,
    ) -> Result<crate::codeintel_policies::CodeIntelligenceRetentionPolicyMatchesConnection> {
        let Some(u) = &self.upload else {
            return Ok(
                crate::codeintel_policies::CodeIntelligenceRetentionPolicyMatchesConnection::empty(
                ),
            );
        };
        crate::codeintel_policies::retention_policy_overview(
            ctx,
            u,
            matches_only,
            query,
            after,
            first,
        )
        .await
    }

    /// Whether no retention policy keeps the index any more; it is deleted once no other
    /// index depends on it.
    async fn expired(&self) -> bool {
        self.upload.as_ref().is_some_and(|u| u.expired)
    }
}

/// A page of precise indexes.
#[derive(SimpleObject)]
pub struct PreciseIndexConnection {
    nodes: Vec<PreciseIndexNode>,
    total_count: Option<i32>,
    page_info: PageInfo,
}

/// The upload and index job ids of a `PreciseIndex` ID (`None` for other IDs).
pub fn parse_precise_index_id(id: &str) -> Option<(Option<i64>, Option<i64>)> {
    let (kind, key) = unmarshal_id(id)?;
    if kind != PRECISE_INDEX_KIND {
        return None;
    }
    let key: String = serde_json::from_str(&key).ok()?;
    let parts: Vec<&str> = key.split(':').collect();
    if parts.is_empty() || parts.len() % 2 != 0 {
        return None;
    }
    let (mut upload, mut job) = (None, None);
    for pair in parts.chunks(2) {
        let n: i64 = pair[1].parse().ok()?;
        match pair[0] {
            "U" => upload = Some(n),
            "I" => job = Some(n),
            _ => return None,
        }
    }
    (upload.is_some() || job.is_some()).then_some((upload, job))
}

/// Attaches the jobs of uploads made by auto-indexing.
async fn with_jobs(ctx: &Context<'_>, uploads: Vec<Upload>) -> Result<Vec<PreciseIndexNode>> {
    let ids: Vec<i64> = uploads
        .iter()
        .filter_map(|u| u.associated_index_id)
        .collect();
    let jobs = if ids.is_empty() {
        Vec::new()
    } else {
        IndexJobStore::new(&services(ctx)?.db)
            .get_many(&ids)
            .await
            .map_err(db_error)?
    };
    Ok(uploads
        .into_iter()
        .map(|u| {
            let job = u
                .associated_index_id
                .and_then(|id| jobs.iter().find(|j| j.id == id).cloned());
            PreciseIndexNode {
                upload: Some(u),
                job,
            }
        })
        .collect())
}

/// Loads a precise index as a node if the viewer can read its repository: by its upload if
/// the ID has one (with the job that made it), else by its job (with its upload, once there
/// is one).
pub async fn load_node(
    ctx: &Context<'_>,
    (upload, job): (Option<i64>, Option<i64>),
) -> Result<Option<PreciseIndexNode>> {
    require_reader(ctx)?;
    let svc = services(ctx)?;
    let node = match (upload, job) {
        (Some(id), _) => match CodeIntelStore::new(&svc.db)
            .get_upload(id)
            .await
            .map_err(db_error)?
        {
            Some(u) => with_jobs(ctx, vec![u]).await?.pop(),
            None => None,
        },
        (None, Some(id)) => match IndexJobStore::new(&svc.db)
            .get(id)
            .await
            .map_err(db_error)?
        {
            Some(j) => {
                let upload = match j.job.associated_upload_id {
                    Some(u) => CodeIntelStore::new(&svc.db)
                        .get_upload(u)
                        .await
                        .map_err(db_error)?,
                    None => None,
                };
                Some(PreciseIndexNode {
                    upload,
                    job: Some(j),
                })
            }
            None => None,
        },
        (None, None) => None,
    };
    let Some(node) = node else {
        return Ok(None);
    };
    let access = repo_access(ctx).await?;
    let visible = svc
        .db
        .repos()
        .get_authorized(node.repo_commit_root().0, &access)
        .await
        .map_err(db_error)?
        .is_some();
    Ok(visible.then_some(node))
}

/// The filter arguments shared by `preciseIndexes` and the bulk mutations.
struct Filter {
    repo: Option<ID>,
    query: Option<String>,
    states: Option<Vec<PreciseIndexState>>,
    indexer_key: Option<String>,
    include_deleted: bool,
    /// Only the dependents of this index.
    dependent_of: Option<i64>,
}

/// The store filters of a [`Filter`]: uploads and index jobs, `None` for a kind that
/// cannot match.
struct Resolved {
    uploads: Option<ListUploads>,
    jobs: Option<ListIndexJobs>,
}

impl Filter {
    /// The store filters; both `None` when nothing can match (a repository that is hidden
    /// or unknown).
    async fn resolve(self, ctx: &Context<'_>, access: &RepoAccess) -> Result<Resolved> {
        let nothing = Resolved {
            uploads: None,
            jobs: None,
        };
        let repo = match self.repo {
            None => None,
            Some(id) => {
                let Ok(repo) = parse_repository_id(&id) else {
                    return Ok(nothing);
                };
                let visible = services(ctx)?
                    .db
                    .repos()
                    .get_authorized(repo, access)
                    .await
                    .map_err(db_error)?
                    .is_some();
                if !visible {
                    return Ok(nothing);
                }
                Some(repo)
            }
        };
        let requested = self.states.unwrap_or_default();
        let upload_states: Vec<UploadState> = requested
            .iter()
            .flat_map(|s| s.upload_states().iter().copied())
            .collect();
        let job_states: Vec<String> = requested
            .iter()
            .flat_map(|s| s.job_states().iter().map(|s| (*s).to_owned()))
            .collect();
        let filtered = !requested.is_empty();
        let mut names = Vec::new();
        if let Some(key) = self
            .indexer_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            names.push(key.to_owned());
            if let Some(k) = KNOWN_INDEXERS.iter().find(|k| k.key == key) {
                names.push(k.name.to_owned());
                names.extend(k.aliases.iter().map(|a| (*a).to_owned()));
            }
        }
        // Uploads may name the indexer by its image.
        let mut upload_names = names.clone();
        upload_names.extend(names.iter().map(|n| format!("sourcegraph/{n}")));
        let uploads = (!filtered || !upload_states.is_empty()).then(|| ListUploads {
            repo,
            states: upload_states,
            query: self.query.clone(),
            indexers: upload_names,
            include_deleted: self.include_deleted,
            dependent_of: self.dependent_of,
            limit: 0,
            offset: 0,
        });
        // Dependency filters only apply to uploads.
        let jobs = ((!filtered || !job_states.is_empty()) && self.dependent_of.is_none())
            .then_some(ListIndexJobs {
                repo,
                states: job_states,
                query: self.query,
                indexers: names,
                without_upload: true,
                limit: 0,
                offset: 0,
            });
        Ok(Resolved { uploads, jobs })
    }
}

/// The kind of the merged cursor of `preciseIndexes`: "<upload offset>:<job offset>".
const CURSOR_KIND: &str = "PreciseIndexCursor";

fn decode_cursor(after: Option<&str>) -> Result<(i64, i64)> {
    let invalid = || error(codes::INVALID_INPUT, "Invalid cursor.");
    let Some(after) = after.filter(|a| !a.is_empty()) else {
        return Ok((0, 0));
    };
    let (kind, key) = unmarshal_id(after).ok_or_else(invalid)?;
    if kind != CURSOR_KIND {
        return Err(invalid());
    }
    let key: String = serde_json::from_str(&key).map_err(|_| invalid())?;
    let (u, i) = key.split_once(':').ok_or_else(invalid)?;
    let parse = |s: &str| -> Result<i64> {
        if s.is_empty() {
            return Ok(i64::MAX);
        }
        s.parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .ok_or_else(invalid)
    };
    Ok((parse(u)?, parse(i)?))
}

#[derive(Default)]
pub struct PreciseIndexesQuery;

#[Object]
impl PreciseIndexesQuery {
    /// Precise code intelligence indexes the viewer can see, newest first: uploads, and
    /// auto-indexing jobs that have not produced one yet.
    #[allow(clippy::too_many_arguments)]
    async fn precise_indexes(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only indexes of this repository.")] repo: Option<ID>,
        #[graphql(desc = "Matches the commit, root, indexer or repository name.")] query: Option<
            String,
        >,
        states: Option<Vec<PreciseIndexState>>,
        indexer_key: Option<String>,
        #[graphql(desc = "Also list deleted indexes.")] include_deleted: Option<bool>,
        #[graphql(
            desc = "Only the indexes that depend on this one (they use a package it defines)."
        )]
        dependent_of: Option<ID>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<PreciseIndexConnection> {
        require_reader(ctx)?;
        let access = repo_access(ctx).await?;
        let dependent_of = match dependent_of {
            None => None,
            Some(id) => match parse_precise_index_id(&id) {
                Some((Some(u), j)) if load_node(ctx, (Some(u), j)).await?.is_some() => Some(u),
                _ => Some(-1),
            },
        };
        let filter = Filter {
            repo,
            query,
            states,
            indexer_key,
            include_deleted: include_deleted.unwrap_or(false),
            dependent_of,
        };
        let resolved = filter.resolve(ctx, &access).await?;
        let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
        let (u_off, j_off) = decode_cursor(after.as_deref())?;
        let svc = services(ctx)?;
        let (mut uploads, u_total) = match resolved.uploads {
            Some(mut opts) if u_off != i64::MAX => {
                opts.limit = limit;
                opts.offset = u_off;
                CodeIntelStore::new(&svc.db)
                    .list_uploads(&opts, &access)
                    .await
                    .map_err(db_error)?
            }
            Some(mut opts) => {
                opts.limit = 0;
                let (_, total) = CodeIntelStore::new(&svc.db)
                    .list_uploads(&opts, &access)
                    .await
                    .map_err(db_error)?;
                (Vec::new(), total)
            }
            None => (Vec::new(), 0),
        };
        let (mut jobs, j_total) = match resolved.jobs {
            Some(mut opts) if j_off != i64::MAX => {
                opts.limit = limit;
                opts.offset = j_off;
                IndexJobStore::new(&svc.db)
                    .list(&opts, &access)
                    .await
                    .map_err(db_error)?
            }
            Some(mut opts) => {
                opts.limit = 0;
                let (_, total) = IndexJobStore::new(&svc.db)
                    .list(&opts, &access)
                    .await
                    .map_err(db_error)?;
                (Vec::new(), total)
            }
            None => (Vec::new(), 0),
        };
        // Merge newest first: uploads by upload time, jobs by queue time.
        let page = usize::try_from(limit).unwrap_or(0);
        uploads.reverse();
        jobs.reverse();
        let (mut taken_u, mut taken_j) = (Vec::new(), Vec::new());
        let mut order = Vec::new();
        while order.len() < page {
            let take_upload = match (uploads.last(), jobs.last()) {
                (Some(u), Some(j)) => u.uploaded_at > j.queued_at,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_upload {
                taken_u.push(uploads.pop().expect("an upload"));
                order.push(true);
            } else {
                taken_j.push(jobs.pop().expect("a job"));
                order.push(false);
            }
        }
        let used_u = taken_u.len() as i64;
        let used_j = taken_j.len() as i64;
        let mut upload_nodes = with_jobs(ctx, taken_u).await?.into_iter();
        let mut job_nodes = taken_j.into_iter();
        let nodes: Vec<PreciseIndexNode> = order
            .into_iter()
            .filter_map(|is_upload| {
                if is_upload {
                    upload_nodes.next()
                } else {
                    job_nodes.next().map(|j| PreciseIndexNode {
                        upload: None,
                        job: Some(j),
                    })
                }
            })
            .collect();
        let next = |off: i64, used: i64, total: i64| {
            let off = if off == i64::MAX { total } else { off };
            (off + used < total).then(|| (off + used).to_string())
        };
        let (nu, nj) = (next(u_off, used_u, u_total), next(j_off, used_j, j_total));
        let end_cursor = (nu.is_some() || nj.is_some()).then(|| {
            marshal_string_id(
                CURSOR_KIND,
                &format!("{}:{}", nu.unwrap_or_default(), nj.unwrap_or_default()),
            )
        });
        Ok(PreciseIndexConnection {
            nodes,
            total_count: Some(to_int(u_total + j_total)),
            page_info: PageInfo {
                has_next_page: end_cursor.is_some(),
                end_cursor,
            },
        })
    }

    /// The indexer keys of the indexes the viewer can see (the `indexerKey` filter values).
    async fn indexer_keys(&self, ctx: &Context<'_>, repo: Option<ID>) -> Result<Vec<String>> {
        require_reader(ctx)?;
        let access = repo_access(ctx).await?;
        let filter = Filter {
            repo: repo.clone(),
            query: None,
            states: None,
            indexer_key: None,
            include_deleted: false,
            dependent_of: None,
        };
        let Some(opts) = filter.resolve(ctx, &access).await?.uploads else {
            return Ok(Vec::new());
        };
        // Without a repository, only site admins see every indexer in use.
        if opts.repo.is_none() && !matches!(access, RepoAccess::All) {
            return Ok(Vec::new());
        }
        let names = CodeIntelStore::new(&services(ctx)?.db)
            .indexers(opts.repo)
            .await
            .map_err(db_error)?;
        let mut keys: Vec<String> = names.iter().map(|n| indexer_key(n).to_owned()).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

#[derive(Default)]
pub struct PreciseIndexesMutation;

/// The index behind a `PreciseIndex` ID, for site admins.
async fn admin_index(ctx: &Context<'_>, id: &ID) -> Result<PreciseIndexNode> {
    require_site_admin(ctx)?;
    let not_found = || error(codes::NOT_FOUND, "Precise index not found.");
    let key = parse_precise_index_id(id).ok_or_else(not_found)?;
    load_node(ctx, key).await?.ok_or_else(not_found)
}

/// Marks a job (and the upload it made) for reindexing and queues it again. Returns the new
/// job's id.
async fn rerun(
    ctx: &Context<'_>,
    job: &IndexJobRow,
    upload: Option<&Upload>,
    by: UserId,
) -> Result<i64> {
    let db = &services(ctx)?.db;
    if let Some(u) = upload {
        CodeIntelStore::new(db)
            .mark_should_reindex(&[u.id])
            .await
            .map_err(db_error)?;
    }
    IndexJobStore::new(db)
        .mark_should_reindex(&[job.id])
        .await
        .map_err(db_error)?;
    let mut again = job.job.clone();
    again.should_reindex = false;
    again.associated_upload_id = None;
    again.enqueuer = Some(by);
    IndexJobStore::new(db)
        .enqueue(&again)
        .await
        .map_err(db_error)
}

impl PreciseIndexesMutation {
    /// The uploads and the jobs without uploads matching a filter, for site admins.
    async fn matching(
        ctx: &Context<'_>,
        query: Option<String>,
        states: Option<Vec<PreciseIndexState>>,
        indexer_key: Option<String>,
        repository: Option<ID>,
    ) -> Result<(Vec<i64>, Vec<i64>)> {
        require_site_admin(ctx)?;
        let filter = Filter {
            repo: repository,
            query,
            states,
            indexer_key,
            include_deleted: false,
            dependent_of: None,
        };
        let resolved = filter.resolve(ctx, &RepoAccess::All).await?;
        let db = &services(ctx)?.db;
        let uploads = match resolved.uploads {
            Some(opts) => CodeIntelStore::new(db)
                .matching_ids(&opts, &RepoAccess::All)
                .await
                .map_err(db_error)?,
            None => Vec::new(),
        };
        let jobs = match resolved.jobs {
            Some(opts) => IndexJobStore::new(db)
                .matching_ids(&opts)
                .await
                .map_err(db_error)?,
            None => Vec::new(),
        };
        Ok((uploads, jobs))
    }

    /// Reindexes: uploads from auto-indexing and jobs are queued again, other uploads are
    /// processed again. Returns how many were reindexed.
    async fn reindex(ctx: &Context<'_>, nodes: Vec<PreciseIndexNode>) -> Result<u64> {
        let actor = require_site_admin(ctx)?;
        let db = &services(ctx)?.db;
        let mut n = 0;
        let mut reprocess = Vec::new();
        for node in nodes {
            match (&node.upload, &node.job) {
                (upload, Some(job)) => {
                    if job.state == "queued" || job.state == "processing" {
                        continue;
                    }
                    rerun(ctx, job, upload.as_ref(), actor.user_id).await?;
                    n += 1;
                }
                (Some(u), None) => reprocess.push(u.id),
                (None, None) => {}
            }
        }
        if !reprocess.is_empty() {
            n += CodeIntelStore::new(db)
                .requeue(&reprocess)
                .await
                .map_err(db_error)?;
        }
        Ok(n)
    }
}

#[Object]
impl PreciseIndexesMutation {
    /// Deletes a precise index (site admins only): an upload's data is removed in the
    /// background; a job without an upload is removed (an executor running it stops).
    async fn delete_precise_index(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let node = admin_index(ctx, &id).await?;
        let db = &services(ctx)?.db;
        match (&node.upload, &node.job) {
            (Some(u), _) => {
                CodeIntelStore::new(db)
                    .mark_deleting(&[u.id], "deleted by a site admin")
                    .await
                    .map_err(db_error)?;
            }
            (None, Some(j)) => {
                IndexJobStore::new(db)
                    .delete(&[j.id])
                    .await
                    .map_err(db_error)?;
            }
            (None, None) => {}
        }
        Ok(EmptyResponse)
    }

    /// Deletes the precise indexes matching the filter (site admins only).
    async fn delete_precise_indexes(
        &self,
        ctx: &Context<'_>,
        query: Option<String>,
        states: Option<Vec<PreciseIndexState>>,
        indexer_key: Option<String>,
        repository: Option<ID>,
    ) -> Result<EmptyResponse> {
        let (uploads, jobs) = Self::matching(ctx, query, states, indexer_key, repository).await?;
        let db = &services(ctx)?.db;
        CodeIntelStore::new(db)
            .mark_deleting(&uploads, "deleted by a site admin")
            .await
            .map_err(db_error)?;
        IndexJobStore::new(db)
            .delete(&jobs)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Reindexes a precise index (site admins only). An index auto-indexing made, or an
    /// index job that finished, is marked `shouldReindex` and its job is queued again; an
    /// upload from elsewhere that completed or failed is processed again.
    async fn reindex_precise_index(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let node = admin_index(ctx, &id).await?;
        if Self::reindex(ctx, vec![node]).await? == 0 {
            return Err(error(
                codes::INVALID_INPUT,
                "Only completed or errored precise indexes can be reindexed.",
            ));
        }
        Ok(EmptyResponse)
    }

    /// Reindexes the completed or errored precise indexes matching the filter (site admins
    /// only).
    async fn reindex_precise_indexes(
        &self,
        ctx: &Context<'_>,
        query: Option<String>,
        states: Option<Vec<PreciseIndexState>>,
        indexer_key: Option<String>,
        repository: Option<ID>,
    ) -> Result<EmptyResponse> {
        let (uploads, jobs) = Self::matching(ctx, query, states, indexer_key, repository).await?;
        let db = &services(ctx)?.db;
        let mut nodes = Vec::new();
        for id in uploads {
            if let Some(u) = CodeIntelStore::new(db)
                .get_upload(id)
                .await
                .map_err(db_error)?
            {
                nodes.extend(with_jobs(ctx, vec![u]).await?);
            }
        }
        for j in IndexJobStore::new(db)
            .get_many(&jobs)
            .await
            .map_err(db_error)?
        {
            nodes.push(PreciseIndexNode {
                upload: None,
                job: Some(j),
            });
        }
        Self::reindex(ctx, nodes).await?;
        Ok(EmptyResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexer_keys() {
        assert_eq!(indexer_key("scip-go"), "go");
        assert_eq!(
            indexer_key("sourcegraph/scip-typescript:latest"),
            "typescript"
        );
        assert_eq!(indexer_key("lsif-tsc"), "typescript");
        assert_eq!(indexer_key("my-indexer"), "my-indexer");
        assert!(CodeIntelIndexer::of("").is_none());
    }

    #[test]
    fn ids() {
        let id = |s: &str| marshal_string_id(PRECISE_INDEX_KIND, s);
        assert_eq!(parse_precise_index_id(&id("U:42")), Some((Some(42), None)));
        assert_eq!(parse_precise_index_id(&id("I:7")), Some((None, Some(7))));
        assert_eq!(
            parse_precise_index_id(&id("U:42:I:7")),
            Some((Some(42), Some(7)))
        );
        assert_eq!(parse_precise_index_id(&id("X:1")), None);
        assert_eq!(parse_precise_index_id(&id("U:")), None);
        assert_eq!(parse_precise_index_id("bogus"), None);
    }

    #[test]
    fn job_states() {
        for s in ["queued", "processing", "errored", "failed", "completed"] {
            assert!(PreciseIndexState::of_job(s).job_states().contains(&s));
        }
    }

    #[test]
    fn states_round_trip() {
        for s in [
            UploadState::Uploading,
            UploadState::Queued,
            UploadState::Processing,
            UploadState::Failed,
            UploadState::Completed,
            UploadState::Deleting,
            UploadState::Deleted,
        ] {
            assert!(PreciseIndexState::of(s).upload_states().contains(&s));
        }
    }
}
