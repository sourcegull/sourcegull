//! Server-side execution of bulk change specs: what `executeBatchSpec`
//! (GraphQL) and the `batches` executor queue (`sourcegull-http-api`) share.
//!
//! - [`execute_batch_spec`] queues one execution job per workspace that is neither skipped nor
//!   fully cached. A fully cached workspace (the resolver found the result of its last step in
//!   the execution cache, `cachedResultFound`) gets its changeset specs right away, built from
//!   the cached result with `sourcegull_batches_steps::build_changeset_specs`, and no job.
//! - [`batches_job`] turns a dequeued job into the executor's [`Job`] with its [`BatchesJob`]
//!   (the contract is documented in [`sourcegull_executor_api`], "The `batches` queue").
//! - [`store_cache_entry`] and [`store_result`] store what an executor uploads with its job
//!   token: step results (in the job user's execution cache, only under the job's own step
//!   cache keys) and changeset specs (validated as `createChangesetSpec` does, of the job's
//!   repository only).
//! - [`execution_finished`] runs when a job reaches a final state: once no job of the spec is
//!   left to run and every one completed, a spec executed with `autoApplyEnabled` is applied
//!   ([`auto_apply`]) through the [`AutoApplier`] set at startup
//!   ([`AppState::auto_applier`](crate::AppState::auto_applier); `applyBatchChange` lives in
//!   the GraphQL crate, which depends on this one).
//! - [`access`]: who may use and manage bulk changes, specs and credentials.

pub mod access;

use std::collections::HashMap;
use std::sync::Arc;

use sourcegull_batches::BatchSpec as ParsedSpec;
use sourcegull_batches::store::{
    BatchSpec, BatchSpecStore, BatchSpecWorkspace, BatchSpecWorkspaceFileStore,
    BatchSpecWorkspaceStore, ChangesetSpec, ChangesetSpecType, ExecutionCacheStore,
    ExecutionJobRow, ListWorkspaces, NewChangesetSpec, Published,
};
use sourcegull_batches_steps::{
    AfterStepResult, CachedStep, ChangesetSpecInput, SpecRepository, build_changeset_specs,
};
use sourcegull_db::{Db, DbError};
use sourcegull_executor_api::{
    self as api, BatchesCachedStep, BatchesCommitAuthor, BatchesJob, BatchesRepository, Job,
};
use sourcegull_pgqueue::JobState;
use sourcegull_types::{Repo, RepoId, UserId};

use crate::relay::{marshal_id, marshal_string_id};

/// Node kind of repository IDs.
const REPOSITORY_KIND: &str = "Repository";
/// Node kind of bulk change spec IDs (keyed by the spec's random id).
const BATCH_SPEC_KIND: &str = "BatchSpec";
/// Node kind of workspace IDs.
const WORKSPACE_KIND: &str = "BatchSpecWorkspace";
/// Node kind of changeset spec IDs (keyed by the spec's random id).
const CHANGESET_SPEC_KIND: &str = "ChangesetSpec";

/// The `fork_namespace` of a spec pushed to a fork in the publishing user's namespace
/// (`changesetTemplate.fork: true`).
pub const FORK_NAMESPACE_USER: &str = "<user>";

/// The GraphQL ID of a repository.
pub fn repository_id(repo: RepoId) -> String {
    marshal_id(REPOSITORY_KIND, i64::from(repo.get()))
}

/// The GraphQL ID of a changeset spec.
pub fn changeset_spec_id(rand_id: &str) -> String {
    marshal_string_id(CHANGESET_SPEC_KIND, rand_id)
}

/// Lines added and deleted by a unified diff (`+`/`-` lines inside hunks).
pub fn diff_stat(diff: &[u8]) -> (i32, i32) {
    let (mut added, mut deleted, mut in_hunk) = (0i32, 0i32, false);
    for line in diff.split(|b| *b == b'\n') {
        if line.starts_with(b"diff ") {
            in_hunk = false;
        } else if line.starts_with(b"@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.first() {
                Some(b'+') => added = added.saturating_add(1),
                Some(b'-') => deleted = deleted.saturating_add(1),
                _ => {}
            }
        }
    }
    (added, deleted)
}

/// Converts a validated changeset spec of `repo` created by `user`.
pub fn new_changeset_spec(
    parsed: sourcegull_batches::ChangesetSpec,
    repo: RepoId,
    user: UserId,
) -> NewChangesetSpec {
    match parsed {
        sourcegull_batches::ChangesetSpec::Existing(e) => {
            let mut s = NewChangesetSpec::new(repo, ChangesetSpecType::Existing);
            s.user_id = Some(user);
            s.external_id = Some(e.external_id);
            s
        }
        sourcegull_batches::ChangesetSpec::Branch(b) => {
            let mut s = NewChangesetSpec::new(repo, ChangesetSpecType::Branch);
            s.user_id = Some(user);
            s.base_ref = Some(b.base_ref.clone());
            s.base_rev = Some(b.base_rev.clone());
            s.head_ref = Some(b.head_ref.clone());
            s.title = Some(b.title.clone());
            s.body = Some(b.body.clone());
            s.published = b.published.map(|p| match p {
                sourcegull_batches::PublishedValue::Published => Published::Bool(true),
                sourcegull_batches::PublishedValue::Unpublished => Published::Bool(false),
                sourcegull_batches::PublishedValue::Draft => Published::Draft,
            });
            if b.fork == Some(true) {
                s.fork_namespace = Some(FORK_NAMESPACE_USER.to_owned());
            }
            if let Some(c) = b.commits.into_iter().next() {
                let (added, deleted) = diff_stat(&c.diff);
                s.diff_stat_added = Some(added);
                s.diff_stat_deleted = Some(deleted);
                s.diff = Some(c.diff);
                s.commit_message = Some(c.message);
                s.commit_author_name = Some(c.author_name);
                s.commit_author_email = Some(c.author_email);
            }
            s
        }
    }
}

/// The step cache key of every step of `spec` in workspace `ws` of repository `repo_name`
/// (step `i` at index `i`), as the resolver and the step runner compute them.
pub fn step_cache_keys(spec: &ParsedSpec, repo_name: &str, ws: &BatchSpecWorkspace) -> Vec<String> {
    (0..spec.steps.len())
        .map(|i| {
            sourcegull_batches::cache_key::step_cache_key(
                spec,
                repo_name,
                &ws.commit,
                &ws.file_matches,
                &ws.path,
                ws.only_fetch_workspace,
                i,
            )
        })
        .collect()
}

/// The cache hits the resolver recorded for a workspace, first step first
/// (`step_cache_results` is `{"<step number from 1>": {"key": …, "value": <CachedStep>}}`).
pub fn cached_steps(ws: &BatchSpecWorkspace) -> Vec<BatchesCachedStep> {
    let Some(results) = ws.step_cache_results.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<BatchesCachedStep> = results
        .iter()
        .filter_map(|(number, entry)| {
            let number: usize = number.parse().ok()?;
            let key = entry.get("key")?.as_str()?.to_owned();
            let value = match entry.get("value")? {
                serde_json::Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            Some(BatchesCachedStep {
                step_index: number.checked_sub(1)?,
                key,
                value,
            })
        })
        .collect();
    out.sort_by_key(|s| s.step_index);
    out
}

/// The result of the last cached step of a fully cached workspace.
fn final_cached_result(ws: &BatchSpecWorkspace) -> Option<AfterStepResult> {
    let last = cached_steps(ws).pop()?;
    CachedStep::from_json(&last.value).ok().map(|c| c.result)
}

/// The commit author when the template has none: the user's display name (or username) and
/// primary email.
pub async fn fallback_author(
    db: &Db,
    user: UserId,
) -> Result<Option<BatchesCommitAuthor>, DbError> {
    let Some(u) = db.users().get_by_id(user).await? else {
        return Ok(None);
    };
    let emails = db.user_emails().list(user).await?;
    let Some(email) = emails
        .iter()
        .find(|e| e.is_primary)
        .or_else(|| emails.iter().find(|e| e.verified_at.is_some()))
        .or_else(|| emails.first())
    else {
        return Ok(None);
    };
    Ok(Some(BatchesCommitAuthor {
        name: u
            .display_name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(u.username),
        email: email.email.clone(),
    }))
}

/// The repository of a workspace as changeset specs name it.
fn spec_repository(repo: &Repo, ws: &BatchSpecWorkspace) -> SpecRepository {
    SpecRepository {
        id: repository_id(repo.id),
        name: repo.name.clone(),
        base_ref: ws.branch.clone(),
        base_rev: ws.commit.clone(),
        file_matches: ws.file_matches.clone(),
    }
}

/// The executor's job for an execution of workspace `ws` (see the protocol docs).
/// `secret_env` and the registry credentials are added by the caller.
pub fn batches_job(
    job_id: i64,
    token: &str,
    spec: &BatchSpec,
    ws: &BatchSpecWorkspace,
    repo: &Repo,
    author: Option<BatchesCommitAuthor>,
) -> Job {
    let branch = ws
        .branch
        .strip_prefix("refs/heads/")
        .unwrap_or(&ws.branch)
        .to_owned();
    Job {
        id: job_id,
        queue: api::BATCHES_QUEUE.into(),
        token: token.to_owned(),
        repository_name: repo.name.clone(),
        commit: ws.commit.clone(),
        shallow_clone: true,
        fetch_tags: false,
        sparse_checkout: if ws.only_fetch_workspace && !ws.path.is_empty() {
            vec![ws.path.clone()]
        } else {
            Vec::new()
        },
        redacted_values: [(token.to_owned(), "JOB_TOKEN_REMOVED".to_owned())].into(),
        batches: Some(BatchesJob {
            version: api::BATCHES_JOB_VERSION,
            batch_spec_id: marshal_string_id(BATCH_SPEC_KIND, &spec.rand_id),
            workspace_id: marshal_id(WORKSPACE_KIND, ws.id),
            raw_spec: spec.raw_spec.clone(),
            repository: BatchesRepository {
                id: repository_id(repo.id),
                name: repo.name.clone(),
                base_ref: ws.branch.clone(),
                branch,
                base_rev: ws.commit.clone(),
                file_matches: ws.file_matches.clone(),
            },
            path: ws.path.clone(),
            only_fetch_workspace: ws.only_fetch_workspace,
            no_cache: spec.no_cache,
            cached_steps: if spec.no_cache {
                Vec::new()
            } else {
                cached_steps(ws)
            },
            fallback_author: author,
            // Filled in by the dequeue (`workspace_files`).
            workspace_files: Vec::new(),
        }),
        ..Default::default()
    }
}

/// The files uploaded with `spec` for its steps to `mount`, as the executor downloads them.
pub async fn workspace_files(
    db: &Db,
    spec: &BatchSpec,
) -> Result<Vec<api::BatchesWorkspaceFile>, DbError> {
    let files = BatchSpecWorkspaceFileStore::new(db)
        .list(spec.id, 0, 0)
        .await?;
    Ok(files
        .iter()
        .map(|f| api::BatchesWorkspaceFile {
            path: f.full_path(),
            url: crate::batch_files::file_path(
                crate::batch_files::EXECUTOR_PREFIX,
                &spec.rand_id,
                &f.rand_id,
            ),
            size: f.size,
            modified_at: f.modified_at,
        })
        .collect())
}

/// The executor secret keys a spec's steps pass through (`env: [NAME]`), sorted.
pub fn pass_through_keys(spec: &ParsedSpec) -> Vec<String> {
    let mut keys: Vec<String> = spec
        .steps
        .iter()
        .flat_map(|s| s.env.outer_vars())
        .map(str::to_owned)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Why `executeBatchSpec` refused.
#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("Only bulk change specs created for server-side execution can be executed.")]
    NotServerSide,
    #[error("The bulk change spec's workspaces are still being resolved.")]
    ResolutionIncomplete,
    #[error("The bulk change spec's workspace resolution failed.")]
    ResolutionFailed,
    #[error("The bulk change spec has already been executed.")]
    AlreadyExecuted,
    #[error("The creator of the bulk change spec has been deleted.")]
    NoCreator,
    #[error("invalid bulk change spec: {0}")]
    InvalidSpec(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Applies a bulk change spec as a user, as `applyBatchChange` does (implemented by the GraphQL
/// API, set with [`AppState::with_auto_applier`](crate::AppState::with_auto_applier)).
#[async_trait::async_trait]
pub trait AutoApplier: Send + Sync + std::fmt::Debug {
    /// Applies `spec` as `user`, with the checks of an apply by that user. Returns the
    /// error message when the apply fails.
    async fn apply(&self, spec: &BatchSpec, user: UserId) -> Result<(), String>;
}

/// Options of [`execute_batch_spec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteOptions {
    /// Ignore cached step results; `None` keeps the spec's own `noCache`.
    pub no_cache: Option<bool>,
    /// Apply the spec once every execution completed.
    pub auto_apply: bool,
}

/// What [`execute_batch_spec`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecuteOutcome {
    /// Execution jobs queued.
    pub jobs: u64,
    /// Workspaces whose changeset specs were built from the cache.
    pub cached: usize,
}

/// `executeBatchSpec`: queues the execution of a resolved server-side spec, run as its
/// creator. Fully cached workspaces get their changeset specs from the cache instead of a
/// job (unless `noCache`); a cached result that no longer builds (e.g. the template changed
/// in a way the cache key does not cover) makes the workspace run. `applier` applies the spec
/// when nothing is left to run and it was executed with `autoApplyEnabled`.
pub async fn execute_batch_spec(
    db: &Db,
    applier: Option<&Arc<dyn AutoApplier>>,
    spec: &BatchSpec,
    opts: ExecuteOptions,
) -> Result<ExecuteOutcome, ExecuteError> {
    if !spec.created_from_raw {
        return Err(ExecuteError::NotServerSide);
    }
    match BatchSpecStore::new(db)
        .resolution_job(spec.id)
        .await?
        .map(|j| j.state)
    {
        Some(JobState::Completed) => {}
        Some(JobState::Failed | JobState::Canceled) => return Err(ExecuteError::ResolutionFailed),
        _ => return Err(ExecuteError::ResolutionIncomplete),
    }
    let user = spec.user_id.ok_or(ExecuteError::NoCreator)?;
    let parsed =
        ParsedSpec::parse(&spec.raw_spec).map_err(|e| ExecuteError::InvalidSpec(e.to_string()))?;
    let no_cache = opts.no_cache.unwrap_or(spec.no_cache);
    if !BatchSpecStore::new(db)
        .mark_execution_requested(spec.id, no_cache, opts.auto_apply)
        .await?
    {
        return Err(ExecuteError::AlreadyExecuted);
    }
    match queue_execution(db, spec, &parsed, user, no_cache).await {
        Ok(outcome) => {
            if outcome.jobs == 0 {
                execution_finished(db, applier, spec.id).await?;
            }
            Ok(outcome)
        }
        Err(e) => {
            // Let the user try again.
            let _ = BatchSpecStore::new(db)
                .clear_execution_requested(spec.id)
                .await;
            Err(e.into())
        }
    }
}

async fn queue_execution(
    db: &Db,
    spec: &BatchSpec,
    parsed: &ParsedSpec,
    user: UserId,
    no_cache: bool,
) -> Result<ExecuteOutcome, DbError> {
    let workspaces = BatchSpecWorkspaceStore::new(db);
    let mut cached = 0;
    if no_cache {
        workspaces.clear_cache_results(spec.id).await?;
    } else {
        let hits = workspaces
            .list(&ListWorkspaces {
                batch_spec_id: spec.id,
                state: Some("cached".into()),
                ..Default::default()
            })
            .await?;
        let author = if parsed
            .changeset_template
            .as_ref()
            .is_some_and(|t| t.commit.author.is_none())
            && !hits.is_empty()
        {
            fallback_author(db, user).await?
        } else {
            None
        };
        let repo_ids: Vec<RepoId> = hits.iter().map(|w| w.repo_id).collect();
        let repos: HashMap<RepoId, Repo> = load_repos(db, &repo_ids).await?;
        for ws in &hits {
            let specs = repos
                .get(&ws.repo_id)
                .and_then(|repo| cached_changeset_specs(parsed, repo, ws, author.as_ref(), user));
            match specs {
                Some(specs) => {
                    workspaces.replace_changeset_specs(ws.id, &specs).await?;
                    cached += 1;
                }
                None => {
                    // Run it instead.
                    workspaces
                        .set_cache_results(ws.id, false, &ws.step_cache_results)
                        .await?;
                }
            }
        }
    }
    let jobs = workspaces.enqueue_executions(spec.id, user).await?;
    Ok(ExecuteOutcome { jobs, cached })
}

async fn load_repos(db: &Db, ids: &[RepoId]) -> Result<HashMap<RepoId, Repo>, DbError> {
    let mut out = HashMap::new();
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        // Executions run as the spec's creator, whose access the resolver checked.
        #[allow(clippy::disallowed_methods)]
        if let Some(repo) = db.repos().get(*id).await? {
            out.insert(*id, repo);
        }
    }
    Ok(out)
}

/// The changeset specs of a fully cached workspace, or `None` when its cached result is
/// unusable.
fn cached_changeset_specs(
    parsed: &ParsedSpec,
    repo: &Repo,
    ws: &BatchSpecWorkspace,
    author: Option<&BatchesCommitAuthor>,
    user: UserId,
) -> Option<Vec<NewChangesetSpec>> {
    let result = final_cached_result(ws)?;
    let fallback = author.map(|a| sourcegull_batches::spec::CommitAuthor {
        name: a.name.clone(),
        email: a.email.clone(),
    });
    let repository = spec_repository(repo, ws);
    let built = build_changeset_specs(&ChangesetSpecInput {
        spec: parsed,
        repository: &repository,
        path: &ws.path,
        result: &result,
        fallback_author: fallback.as_ref(),
    })
    .map_err(|e| {
        tracing::warn!(workspace = ws.id, error = %e,
            "building changeset specs from a cached result failed; executing the workspace");
    })
    .ok()?;
    Some(
        built
            .into_iter()
            .map(|s| new_changeset_spec(s, repo.id, user))
            .collect(),
    )
}

/// Called after an execution job of `batch_spec_id` reached a final state (and when an
/// execution needed no job): applies the spec once nothing is left to run, every execution
/// completed and the spec was executed with `autoApplyEnabled`. Returns whether the
/// execution is over.
pub async fn execution_finished(
    db: &Db,
    applier: Option<&Arc<dyn AutoApplier>>,
    batch_spec_id: i64,
) -> Result<bool, DbError> {
    let Some(spec) = BatchSpecStore::new(db).get(batch_spec_id).await? else {
        return Ok(false);
    };
    if spec.execution_requested_at.is_none() {
        return Ok(false);
    }
    let stats = BatchSpecWorkspaceStore::new(db)
        .stats(batch_spec_id)
        .await?;
    if stats.queued + stats.processing > 0 {
        return Ok(false);
    }
    let all_completed = stats.completed == stats.executions;
    if all_completed && spec.auto_apply_enabled && spec.auto_applied_at.is_none() {
        auto_apply(db, applier, &spec).await?;
    }
    Ok(true)
}

/// Applies a spec whose execution completed (`autoApplyEnabled`) as its creator, as
/// `applyBatchChange` would. [`sourcegull_batches::store::BatchSpecStore::claim_auto_apply`] makes sure one
/// caller applies it; a failed apply is logged and not tried again (the user can still apply
/// the spec). Without an applier (or a creator) the spec stays unclaimed.
pub async fn auto_apply(
    db: &Db,
    applier: Option<&Arc<dyn AutoApplier>>,
    spec: &BatchSpec,
) -> Result<(), DbError> {
    let (Some(applier), Some(user)) = (applier, spec.user_id) else {
        tracing::warn!(
            batch_spec = spec.id,
            "the bulk change spec asks to be applied automatically, but it cannot be applied here"
        );
        return Ok(());
    };
    if !BatchSpecStore::new(db).claim_auto_apply(spec.id).await? {
        return Ok(());
    }
    match applier.apply(spec, user).await {
        Ok(()) => tracing::info!(
            batch_spec = spec.id,
            "applied the bulk change spec automatically"
        ),
        Err(e) => tracing::warn!(batch_spec = spec.id, error = %e,
            "applying the bulk change spec automatically failed"),
    }
    Ok(())
}

/// Why an upload of a batches job was refused.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The request is wrong (400).
    #[error("{0}")]
    Invalid(String),
    /// The job is not running any more, or its workspace or spec is gone (404).
    #[error("{0}")]
    Gone(&'static str),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// A running batches job with what it works on.
pub struct RunningJob {
    pub job: ExecutionJobRow,
    pub workspace: BatchSpecWorkspace,
    pub spec: BatchSpec,
    pub parsed: ParsedSpec,
    pub repo: Repo,
}

/// Loads a processing job of the `batches` queue with its workspace, spec and repository.
pub async fn running_job(db: &Db, job_id: i64) -> Result<RunningJob, UploadError> {
    let store = BatchSpecWorkspaceStore::new(db);
    let job = store
        .execution_job(job_id)
        .await?
        .filter(|j| j.state == JobState::Processing)
        .ok_or(UploadError::Gone("The job is not running."))?;
    let workspace = store
        .get(job.payload.batch_spec_workspace_id)
        .await?
        .ok_or(UploadError::Gone("The workspace was deleted."))?;
    let spec = BatchSpecStore::new(db)
        .get(workspace.batch_spec_id)
        .await?
        .ok_or(UploadError::Gone("The bulk change spec was deleted."))?;
    let parsed = ParsedSpec::parse(&spec.raw_spec)
        .map_err(|_| UploadError::Gone("The bulk change spec is no longer valid."))?;
    // The job's repository; executors are trusted with it through the job token.
    #[allow(clippy::disallowed_methods)]
    let repo = db
        .repos()
        .get(workspace.repo_id)
        .await?
        .ok_or(UploadError::Gone("The repository was deleted."))?;
    Ok(RunningJob {
        job,
        workspace,
        spec,
        parsed,
        repo,
    })
}

/// Stores a step result an executor uploaded in the execution cache of the job's user. The
/// key must be one of the job's step cache keys and the value a [`CachedStep`] of that step.
pub async fn store_cache_entry(
    db: &Db,
    job: &RunningJob,
    key: &str,
    value: &str,
) -> Result<(), UploadError> {
    let keys = step_cache_keys(&job.parsed, &job.repo.name, &job.workspace);
    let Some(step) = keys.iter().position(|k| k == key) else {
        return Err(UploadError::Invalid(
            "The key is not a step cache key of this job.".into(),
        ));
    };
    let entry = CachedStep::from_json(value)
        .map_err(|e| UploadError::Invalid(format!("The value is not a cached step result: {e}")))?;
    if entry.result.step_index != step {
        return Err(UploadError::Invalid(format!(
            "The value is the result of step {}, the key of step {}.",
            entry.result.step_index + 1,
            step + 1
        )));
    }
    ExecutionCacheStore::new(db)
        .set(
            job.job.payload.user_id,
            key,
            value,
            i32::try_from(sourcegull_batches_steps::result::RESULT_VERSION).unwrap_or(i32::MAX),
        )
        .await?;
    Ok(())
}

/// Stores the changeset specs a workspace execution produced (replacing earlier uploads of
/// the workspace), attached to the job's bulk change spec and owned by the job's user. Each must be
/// a valid branch changeset spec of the job's repository.
pub async fn store_result(
    db: &Db,
    job: &RunningJob,
    raw_specs: &[String],
) -> Result<Vec<ChangesetSpec>, UploadError> {
    let repo_id = repository_id(job.repo.id);
    let mut news = Vec::with_capacity(raw_specs.len());
    for (i, raw) in raw_specs.iter().enumerate() {
        let parsed = sourcegull_batches::parse_changeset_spec(raw)
            .map_err(|e| UploadError::Invalid(format!("changeset spec {}: {e}", i + 1)))?;
        if parsed.is_importing_existing() {
            return Err(UploadError::Invalid(format!(
                "changeset spec {}: executions create branch changeset specs only",
                i + 1
            )));
        }
        if parsed.base_repository() != repo_id {
            return Err(UploadError::Invalid(format!(
                "changeset spec {}: the repository is not the job's repository",
                i + 1
            )));
        }
        news.push(new_changeset_spec(
            parsed,
            job.repo.id,
            job.job.payload.user_id,
        ));
    }
    Ok(BatchSpecWorkspaceStore::new(db)
        .replace_changeset_specs(job.workspace.id, &news)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_stats() {
        let diff = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n+d\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +0,0 @@\n-gone\n";
        assert_eq!(diff_stat(diff), (2, 2));
        assert_eq!(diff_stat(b""), (0, 0));
    }

    #[test]
    fn cached_steps_from_resolution() {
        let ws = BatchSpecWorkspace {
            id: 1,
            batch_spec_id: 1,
            repo_id: RepoId(1),
            branch: "refs/heads/main".into(),
            commit: "c".into(),
            path: String::new(),
            file_matches: Vec::new(),
            only_fetch_workspace: false,
            ignored: false,
            unsupported: false,
            skipped: false,
            skipped_reason: None,
            cached_result_found: false,
            step_cache_results: serde_json::json!({
                "2": {"key": "k2", "value": {"fingerprint": "f"}},
                "1": {"key": "k1", "value": "{}"},
                "x": {"key": "bad"},
            }),
            changeset_spec_ids: Vec::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let steps = cached_steps(&ws);
        assert_eq!(
            steps
                .iter()
                .map(|s| (s.step_index, s.key.as_str()))
                .collect::<Vec<_>>(),
            [(0, "k1"), (1, "k2")]
        );
        assert_eq!(steps[1].value, r#"{"fingerprint":"f"}"#);
    }
}
