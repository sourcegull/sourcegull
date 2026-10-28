//! The executor queue API and git checkouts for executors: `/.executors/*`.
//!
//! The protocol is described in [`sourcegull_executor_api`]. Queue calls need the shared executor
//! token (site config `executors.accessToken`); without one configured they answer 503. Each
//! dequeued job gets a job token (only its SHA-256 is stored) that grants, for the job's
//! repository only, `GET /.executors/git/{repo}/info/refs?service=git-upload-pack` and
//! `POST /.executors/git/{repo}/git-upload-pack` (git's smart HTTP protocol, served by the
//! repo node's `UploadPack`) and the SCIP upload of the job's index. The token is deleted
//! when the job finishes.
//!
//! Queues: `codeintel` ([`sourcegull_codeintel_scip::store::IndexJob`] rows of `codeintel_index_jobs`) and `batches`
//! ([`sourcegull_batches::store::ExecutionJob`] rows of `batch_spec_workspace_execution_jobs`, dequeued fairly
//! between users, see [`sourcegull_services::batches`]). The `batches` queue's uploads
//! (`/.executors/queue/batches/addCacheEntry`, `/uploadResult`) authenticate with the job token and
//! accept bodies up to [`BATCHES_UPLOAD_LIMIT`]; so do the downloads of the files a spec's steps
//! mount (`GET /.executors/files/batch-changes/{spec}/{file}`, see [`crate::batch_files`]), listed
//! in the job's `workspaceFiles`. A batches job whose cancellation was requested is recorded as
//! `canceled` when the executor marks it errored or failed.
//!
//! Executor secrets: a dequeued index job's `requested_env_vars` are resolved against the
//! global `CODEINTEL` executor secrets (auto-indexing has no user
//! namespace), and the `DOCKER_AUTH_CONFIG` secret, if any, becomes the job's registry
//! credentials. Each value read is recorded in `executor_secret_access_logs` (machine user
//! `codeintel-autoindexing`, with the job id) and as an `ExecutorSecretRead` security event,
//! and each value is redacted from the job's logs as `${{ secrets.NAME }}`. Executors without
//! the `executor-secrets` capability only get jobs that request no secrets.
//!
//! A batches job's secrets are the `BATCHES` executor secrets of the bulk change spec's namespace
//! (its own, then the global ones) that the steps pass through (`env: [NAME]`), plus
//! `DOCKER_AUTH_CONFIG`; they are read as the job's user (the spec's creator), recorded in
//! `executor_secret_access_logs` with that user and as an `ExecutorSecretRead` event with the
//! job id, and redacted the same way.

// Executors are trusted internal callers; jobs reference repositories by id, and git access is
// checked against the job token's repository rather than a viewer's permissions.
#![allow(clippy::disallowed_methods)]
// Handlers return early with ready-made error responses.
#![allow(clippy::result_large_err)]

use std::io::Read as _;
use std::time::Duration;

use async_trait::async_trait;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sourcegull_batches::store::{
    BATCH_SPEC_EXECUTION_JOBS_TABLE, BatchSpecStore, BatchSpecWorkspaceStore,
    EXECUTION_JOB_VERSION, ExecutionJob,
};
use sourcegull_codeintel_scip::store::{INDEX_JOBS_TABLE, IndexJob, IndexJobStore};
use sourcegull_db::{
    ActiveJob, DbError, ExecutorInfo, ExecutorSecretScope, JobTokenScope, ListExecutorSecrets,
    Namespace, SecretAccessor,
};
use sourcegull_executor_api::{
    self as api, AddLogEntryRequest, AddLogEntryResponse, BatchesCacheEntryRequest,
    BatchesResultRequest, BatchesResultResponse, DequeueRequest, DockerAuthConfig, DockerStep,
    HeartbeatRequest, HeartbeatResponse, Job, LogEntry, MarkCompleteRequest, MarkErroredRequest,
    QueueJobIds, UpdateLogEntryRequest, UploadStep, capability,
};
use sourcegull_http_extract::error_response;
use sourcegull_pgqueue::{ExecutionLogEntry, Heartbeat, Queue, Record};
use sourcegull_services::AppState;
use sourcegull_services::git::GitReader;
use subtle::ConstantTimeEq;

/// The server's version, compared with the executor's for skew.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Jobs started less than this long ago are not reclaimed when an executor's heartbeat does
/// not list them: the dequeue response may still be on its way.
const ORPHAN_GRACE: Duration = Duration::from_secs(60);

/// Largest git negotiation request, as the repo node's `UploadPack` accepts
/// (`sourcegull_gitstore::upload_pack::MAX_REQUEST`).
const MAX_GIT_REQUEST: usize = 2 * 1024 * 1024;

/// Prefix of job tokens, so they are recognizable in logs and secret scanners.
const JOB_TOKEN_PREFIX: &str = "sjt_";

/// The executor secret holding registry credentials for image pulls.
pub const DOCKER_AUTH_CONFIG_KEY: &str = "DOCKER_AUTH_CONFIG";

/// In whose name auto-indexing reads executor secrets (a machine user).
pub const CODEINTEL_MACHINE_USER: &str = "codeintel-autoindexing";

/// Security event recorded when a job's executor secrets are read.
pub const SECRET_READ_EVENT: &str = "ExecutorSecretRead";

/// Largest body of a `batches` upload (a step result or changeset specs carry whole diffs).
pub const BATCHES_UPLOAD_LIMIT: usize = 64 * 1024 * 1024;

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "executor API call failed");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Internal error.",
    )
}

fn not_found(message: &str) -> Response {
    error_response(StatusCode::NOT_FOUND, "NOT_FOUND", message)
}

fn unauthorized(message: &str) -> Response {
    error_response(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message)
}

/// Checks the shared executor token.
fn check_executor_token(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let config = state.config.load();
    let Some(expected) = config
        .value
        .executors_access_token
        .as_deref()
        .filter(|t| t.len() >= 20)
    else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            "Executors are not configured: set executors.accessToken in the site config.",
        ));
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(char::is_whitespace))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(api::EXECUTOR_TOKEN_SCHEME))
        .map(|(_, token)| token.trim());
    // Hash both sides so the comparison does not leak the length either.
    let ok = given.is_some_and(|given| {
        Sha256::digest(given.as_bytes())
            .ct_eq(&Sha256::digest(expected.as_bytes()))
            .into()
    });
    if ok {
        Ok(())
    } else {
        Err(unauthorized("Invalid executor token."))
    }
}

/// SHA-256 of a job token.
fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The job behind an `Authorization: Bearer <job token>` header: `Ok(None)` without such a
/// header, 401 for an unknown token.
pub(crate) async fn job_from_bearer(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<JobTokenScope>, Response> {
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(char::is_whitespace))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
    else {
        return Ok(None);
    };
    if !token.starts_with(JOB_TOKEN_PREFIX) {
        return Err(unauthorized("Invalid job token."));
    }
    match state.db.executors().job_token(&token_hash(token)).await {
        Ok(Some(scope)) => Ok(Some(scope)),
        Ok(None) => Err(unauthorized("Invalid or expired job token.")),
        Err(e) => Err(internal(e)),
    }
}

fn to_entry(e: LogEntry) -> ExecutionLogEntry {
    ExecutionLogEntry {
        key: e.key,
        command: e.command,
        start_time: e.start_time,
        exit_code: e.exit_code,
        out: e.out,
        duration_ms: e.duration_ms,
    }
}

type QueueResult<T> = Result<T, sourcegull_pgqueue::Error>;

/// One executor queue: the pgqueue operations plus turning a dequeued row into a [`Job`].
#[async_trait]
trait ExecutorQueue: Send + Sync {
    /// The capabilities an executor needs for this queue's jobs.
    fn required_capabilities(&self) -> &'static [&'static str];
    /// Claims a job for `executor` and builds it (with a fresh job token). `secrets`: the
    /// executor can inject executor secrets.
    async fn dequeue(
        &self,
        state: &AppState,
        executor: &str,
        secrets: bool,
    ) -> Result<Option<Job>, Response>;
    /// Whether `executor` holds job `id`.
    async fn held_by(&self, id: i64, executor: &str) -> QueueResult<bool>;
    async fn heartbeat(&self, ids: &[i64], executor: &str) -> QueueResult<Heartbeat>;
    async fn reset_orphans(&self, executor: &str, running: &[i64]) -> QueueResult<Vec<i64>>;
    async fn add_log(&self, id: i64, entry: &ExecutionLogEntry) -> QueueResult<Option<i32>>;
    async fn update_log(&self, id: i64, index: i32, entry: &ExecutionLogEntry)
    -> QueueResult<bool>;
    async fn complete(&self, id: i64, executor: &str) -> QueueResult<bool>;
    async fn errored(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool>;
    async fn failed(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool>;
    /// Runs after job `id` reached a final state through the executor.
    async fn finished(&self, _state: &AppState, _id: i64) -> Result<(), DbError> {
        Ok(())
    }
}

/// The queue named `name`, if there is one.
fn queue(state: &AppState, name: &str) -> Option<Box<dyn ExecutorQueue>> {
    match name {
        api::CODEINTEL_QUEUE => {
            let pool = state.db.pool().clone();
            let queue = Queue::new(pool.clone(), IndexJobStore::queue_config()).ok()?;
            let without_secrets = Queue::new(
                pool,
                IndexJobStore::queue_config()
                    .with_dequeue_filter("cardinality(requested_env_vars)::text = $3", "0"),
            )
            .ok()?;
            Some(Box::new(CodeIntelQueue {
                queue,
                without_secrets,
            }) as Box<dyn ExecutorQueue>)
        }
        api::BATCHES_QUEUE => {
            let queue = Queue::new(
                state.db.pool().clone(),
                BatchSpecWorkspaceStore::execution_queue_config(EXECUTION_JOB_VERSION),
            )
            .ok()?;
            Some(Box::new(BatchesQueue { queue }) as Box<dyn ExecutorQueue>)
        }
        _ => None,
    }
}

/// The table behind a queue, for job token cleanup.
fn queue_table(name: &str) -> Option<&'static str> {
    match name {
        api::CODEINTEL_QUEUE => Some(INDEX_JOBS_TABLE),
        api::BATCHES_QUEUE => Some(BATCH_SPEC_EXECUTION_JOBS_TABLE),
        _ => None,
    }
}

/// `codeintel`: auto-indexing jobs.
struct CodeIntelQueue {
    queue: Queue<IndexJob>,
    /// The same queue, limited to jobs that request no executor secrets.
    without_secrets: Queue<IndexJob>,
}

/// Executor secrets resolved for one job.
#[derive(Debug, Default)]
pub(crate) struct ResolvedSecrets {
    /// `(key, value)` of the requested secrets that exist.
    pub env: Vec<(String, String)>,
    pub docker_auth_config: Option<DockerAuthConfig>,
}

impl ResolvedSecrets {
    /// Adds the secrets to `job`: the environment, the redactions and the registry
    /// credentials.
    pub(crate) fn apply(self, job: &mut Job) {
        for (key, value) in self.env {
            job.redacted_values
                .insert(value.clone(), format!("${{{{ secrets.{key} }}}}"));
            job.secret_env.push(format!("{key}={value}"));
        }
        if let Some(config) = &self.docker_auth_config {
            for auth in config.auths.values() {
                job.redacted_values.insert(
                    auth.auth.clone(),
                    "${{ secrets.DOCKER_AUTH_CONFIG }}".into(),
                );
            }
        }
        job.docker_auth_config = self.docker_auth_config;
    }
}

/// Why a job's secrets could not be resolved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SecretsError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the DOCKER_AUTH_CONFIG executor secret is not a valid Docker config: {0}")]
    DockerAuthConfig(serde_json::Error),
}

/// Resolves an index job's requested secrets and the registry credentials against the global
/// `CODEINTEL` secrets, recording every read.
pub(crate) async fn resolve_codeintel_secrets(
    state: &AppState,
    job_id: i64,
    requested: &[String],
) -> Result<ResolvedSecrets, SecretsError> {
    resolve_secrets(
        state,
        SecretRequest {
            scope: ExecutorSecretScope::CodeIntel,
            // Auto-indexing only has the global namespace.
            namespace: None,
            by: SecretAccessor::Machine {
                name: CODEINTEL_MACHINE_USER,
                job_id: Some(job_id),
            },
            queue: api::CODEINTEL_QUEUE,
            job_id,
        },
        requested,
    )
    .await
}

/// Where and as whom [`resolve_secrets`] reads.
pub(crate) struct SecretRequest<'a> {
    pub scope: ExecutorSecretScope,
    /// The namespace whose secrets (then the global ones) apply; `None` for global only.
    pub namespace: Option<Namespace>,
    pub by: SecretAccessor<'a>,
    pub queue: &'static str,
    pub job_id: i64,
}

/// Resolves a job's requested secrets and the registry credentials, recording every read in
/// the access logs and as one security event.
pub(crate) async fn resolve_secrets(
    state: &AppState,
    req: SecretRequest<'_>,
    requested: &[String],
) -> Result<ResolvedSecrets, SecretsError> {
    let store = state.db.executor_secrets(state.keyring.clone());
    let mut keys: Vec<String> = requested.to_vec();
    keys.push(DOCKER_AUTH_CONFIG_KEY.to_owned());
    keys.sort_unstable();
    keys.dedup();
    let secrets = store
        .list(
            req.scope,
            &ListExecutorSecrets {
                namespace: req.namespace,
                keys: Some(keys),
                ..Default::default()
            },
        )
        .await?;
    let by = req.by;
    let job_id = req.job_id;
    let mut resolved = ResolvedSecrets::default();
    let mut read = Vec::new();
    for secret in &secrets {
        let value = store.value(secret, by).await?;
        read.push(serde_json::json!({ "id": secret.id, "key": secret.key }));
        if secret.key == DOCKER_AUTH_CONFIG_KEY {
            resolved.docker_auth_config =
                Some(serde_json::from_str(&value).map_err(SecretsError::DockerAuthConfig)?);
        }
        if requested.contains(&secret.key) {
            resolved.env.push((secret.key.clone(), value));
        }
    }
    if !read.is_empty() {
        let (user, mut argument) = match by {
            SecretAccessor::User(u) => (Some(u), serde_json::json!({})),
            SecretAccessor::Machine { name, .. } => {
                (None, serde_json::json!({ "machineUser": name }))
            }
        };
        argument["queue"] = req.queue.into();
        argument["jobId"] = job_id.into();
        argument["secrets"] = read.into();
        state
            .auth
            .log_event(SECRET_READ_EVENT, user, argument)
            .await;
    }
    Ok(resolved)
}

/// The executor job for an index job: the pre-index steps, then the
/// indexer (its local steps, then its command line) in the indexer image, then the upload.
pub(crate) fn index_job_to_job(id: i64, repo_name: &str, job: &IndexJob, token: &str) -> Job {
    let mut steps: Vec<DockerStep> = job
        .docker_steps
        .iter()
        .enumerate()
        .map(|(i, s)| DockerStep {
            key: format!("step.docker.pre-index.{i}"),
            image: s.image.clone(),
            commands: s.commands.clone(),
            dir: s.root.clone(),
            env: Vec::new(),
        })
        .collect();
    if !job.indexer.is_empty() {
        let mut commands = job.local_steps.clone();
        if !job.indexer_args.is_empty() {
            commands.push(job.indexer_args.join(" "));
        }
        steps.push(DockerStep {
            key: "step.docker.indexer".into(),
            image: job.indexer.clone(),
            commands,
            dir: job.root.clone(),
            env: Vec::new(),
        });
    }
    let outfile = if job.outfile.is_empty() {
        "index.scip"
    } else {
        job.outfile.trim_start_matches('/')
    };
    Job {
        id,
        queue: api::CODEINTEL_QUEUE.into(),
        token: token.to_owned(),
        repository_name: repo_name.to_owned(),
        commit: job.commit.clone(),
        shallow_clone: true,
        fetch_tags: false,
        sparse_checkout: Vec::new(),
        docker_steps: steps,
        upload: Some(UploadStep {
            key: "step.upload".into(),
            file: format!("{}{outfile}", job.root),
            root: job.root.clone(),
        }),
        redacted_values: [(token.to_owned(), "JOB_TOKEN_REMOVED".to_owned())].into(),
        ..Default::default()
    }
}

/// At most this many tags of a commit go into a job.
const MAX_COMMIT_TAGS: usize = 100;

/// Names of the tags pointing at `commit` (annotated tags peeled), sorted. Empty when the repo
/// node cannot list them: the job then runs without tags.
async fn commit_tags(state: &AppState, repo: &sourcegull_types::Repo, commit: &str) -> Vec<String> {
    let git = GitReader::new(state.repo_clients.clone(), repo);
    match git.list_refs(false, true, "").await {
        Ok(refs) => {
            let mut tags: Vec<String> = refs
                .refs
                .into_iter()
                .filter(|r| r.commit_oid.eq_ignore_ascii_case(commit))
                .map(|r| r.short_name)
                .filter(|name| api::valid_tag_name(name))
                .collect();
            tags.sort_unstable();
            tags.truncate(MAX_COMMIT_TAGS);
            tags
        }
        Err(e) => {
            tracing::warn!(repo = %repo.name, commit, error = %e,
                "listing the tags of an index job's commit failed; running it without tags");
            Vec::new()
        }
    }
}

#[async_trait]
impl ExecutorQueue for CodeIntelQueue {
    fn required_capabilities(&self) -> &'static [&'static str] {
        &[capability::DOCKER_STEPS, capability::SCIP_UPLOAD]
    }

    async fn dequeue(
        &self,
        state: &AppState,
        executor: &str,
        secrets: bool,
    ) -> Result<Option<Job>, Response> {
        let queue = if secrets {
            &self.queue
        } else {
            &self.without_secrets
        };
        loop {
            let Some(record): Option<Record<IndexJob>> =
                queue.dequeue(executor, 1).await.map_err(internal)?.pop()
            else {
                return Ok(None);
            };
            let repo = state
                .db
                .repos()
                .get(record.payload.repo_id)
                .await
                .map_err(internal)?;
            let Some(repo) = repo else {
                self.queue
                    .mark_failed(record.id, executor, "The repository was deleted.")
                    .await
                    .map_err(internal)?;
                continue;
            };
            let resolved = if secrets {
                match resolve_codeintel_secrets(
                    state,
                    record.id,
                    &record.payload.requested_env_vars,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e @ SecretsError::DockerAuthConfig(_)) => {
                        self.queue
                            .mark_failed(record.id, executor, &e.to_string())
                            .await
                            .map_err(internal)?;
                        continue;
                    }
                    Err(e) => {
                        // Retried later; the executor gets nothing this time.
                        self.queue
                            .mark_errored(record.id, executor, "Reading executor secrets failed.")
                            .await
                            .map_err(internal)?;
                        return Err(internal(e));
                    }
                }
            } else {
                ResolvedSecrets::default()
            };
            let token = new_job_token();
            let scope = JobTokenScope {
                queue: api::CODEINTEL_QUEUE.into(),
                job_id: record.id,
                repo_id: repo.id,
            };
            state
                .db
                .executors()
                .create_job_token(&scope, &token_hash(&token))
                .await
                .map_err(internal)?;
            let mut job = index_job_to_job(record.id, &repo.name, &record.payload, &token);
            job.commit_tags = commit_tags(state, &repo, &job.commit).await;
            resolved.apply(&mut job);
            return Ok(Some(job));
        }
    }

    async fn held_by(&self, id: i64, executor: &str) -> QueueResult<bool> {
        Ok(self.queue.get(id).await?.is_some_and(|r| {
            r.state == sourcegull_pgqueue::JobState::Processing && r.worker_hostname == executor
        }))
    }

    async fn heartbeat(&self, ids: &[i64], executor: &str) -> QueueResult<Heartbeat> {
        self.queue.heartbeat(ids, executor).await
    }

    async fn reset_orphans(&self, executor: &str, running: &[i64]) -> QueueResult<Vec<i64>> {
        self.queue
            .reset_orphans(executor, running, ORPHAN_GRACE)
            .await
    }

    async fn add_log(&self, id: i64, entry: &ExecutionLogEntry) -> QueueResult<Option<i32>> {
        self.queue.add_execution_log(id, entry).await
    }

    async fn update_log(
        &self,
        id: i64,
        index: i32,
        entry: &ExecutionLogEntry,
    ) -> QueueResult<bool> {
        self.queue.update_execution_log(id, index, entry).await
    }

    async fn complete(&self, id: i64, executor: &str) -> QueueResult<bool> {
        self.queue.mark_complete(id, executor).await
    }

    async fn errored(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool> {
        Ok(self
            .queue
            .mark_errored(id, executor, message)
            .await?
            .is_some())
    }

    async fn failed(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool> {
        self.queue.mark_failed(id, executor, message).await
    }
}

/// `batches`: workspace executions of server-side bulk change specs.
struct BatchesQueue {
    queue: Queue<ExecutionJob>,
}

impl BatchesQueue {
    /// Builds the executor's job for a dequeued record; `Ok(None)` when the record was
    /// failed instead (its workspace, spec or repository is gone, or the spec no longer
    /// parses).
    async fn build(
        &self,
        state: &AppState,
        executor: &str,
        record: &Record<ExecutionJob>,
    ) -> Result<Option<Job>, Response> {
        let db = &state.db;
        let fail = |message: &'static str| async move {
            self.queue
                .mark_failed(record.id, executor, message)
                .await
                .map_err(internal)?;
            Ok::<_, Response>(None)
        };
        let Some(ws) = BatchSpecWorkspaceStore::new(db)
            .get(record.payload.batch_spec_workspace_id)
            .await
            .map_err(internal)?
        else {
            return fail("The workspace was deleted.").await;
        };
        let Some(spec) = BatchSpecStore::new(db)
            .get(ws.batch_spec_id)
            .await
            .map_err(internal)?
        else {
            return fail("The bulk change spec was deleted.").await;
        };
        let Some(repo) = db.repos().get(ws.repo_id).await.map_err(internal)? else {
            return fail("The repository was deleted.").await;
        };
        let Ok(parsed) = sourcegull_batches::BatchSpec::parse(&spec.raw_spec) else {
            return fail("The bulk change spec is not valid.").await;
        };
        let user = record.payload.user_id;
        let resolved = match resolve_secrets(
            state,
            SecretRequest {
                scope: ExecutorSecretScope::Batches,
                namespace: Some(spec.namespace),
                by: SecretAccessor::User(user),
                queue: api::BATCHES_QUEUE,
                job_id: record.id,
            },
            &sourcegull_services::batches::pass_through_keys(&parsed),
        )
        .await
        {
            Ok(r) => r,
            Err(e @ SecretsError::DockerAuthConfig(_)) => {
                self.queue
                    .mark_failed(record.id, executor, &e.to_string())
                    .await
                    .map_err(internal)?;
                return Ok(None);
            }
            Err(e) => {
                self.queue
                    .mark_errored(record.id, executor, "Reading executor secrets failed.")
                    .await
                    .map_err(internal)?;
                return Err(internal(e));
            }
        };
        let author = sourcegull_services::batches::fallback_author(db, user)
            .await
            .map_err(internal)?;
        let token = new_job_token();
        let scope = JobTokenScope {
            queue: api::BATCHES_QUEUE.into(),
            job_id: record.id,
            repo_id: repo.id,
        };
        db.executors()
            .create_job_token(&scope, &token_hash(&token))
            .await
            .map_err(internal)?;
        let mut job =
            sourcegull_services::batches::batches_job(record.id, &token, &spec, &ws, &repo, author);
        if let Some(b) = job.batches.as_mut() {
            b.workspace_files = sourcegull_services::batches::workspace_files(db, &spec)
                .await
                .map_err(internal)?;
        }
        resolved.apply(&mut job);
        Ok(Some(job))
    }

    /// The bulk change spec of job `id`.
    async fn spec_of(&self, state: &AppState, id: i64) -> Result<Option<i64>, DbError> {
        let Some(job) = BatchSpecWorkspaceStore::new(&state.db)
            .execution_job(id)
            .await?
        else {
            return Ok(None);
        };
        Ok(BatchSpecWorkspaceStore::new(&state.db)
            .get(job.payload.batch_spec_workspace_id)
            .await?
            .map(|w| w.batch_spec_id))
    }

    /// Ends a job the executor gave up on: `canceled` when its cancellation was requested.
    async fn give_up(
        &self,
        id: i64,
        executor: &str,
        message: &str,
        retry: bool,
    ) -> QueueResult<bool> {
        if self.queue.get(id).await?.is_some_and(|r| r.cancel) {
            return self.queue.mark_canceled(id, executor).await;
        }
        if retry {
            Ok(self
                .queue
                .mark_errored(id, executor, message)
                .await?
                .is_some())
        } else {
            self.queue.mark_failed(id, executor, message).await
        }
    }
}

#[async_trait]
impl ExecutorQueue for BatchesQueue {
    fn required_capabilities(&self) -> &'static [&'static str] {
        &[capability::BATCH_STEPS, capability::SECRETS]
    }

    async fn dequeue(
        &self,
        state: &AppState,
        executor: &str,
        _secrets: bool,
    ) -> Result<Option<Job>, Response> {
        loop {
            let Some(record) = self
                .queue
                .dequeue(executor, 1)
                .await
                .map_err(internal)?
                .pop()
            else {
                return Ok(None);
            };
            if let Some(job) = self.build(state, executor, &record).await? {
                return Ok(Some(job));
            }
            if let Some(spec) = self.spec_of(state, record.id).await.map_err(internal)? {
                sourcegull_services::batches::execution_finished(
                    &state.db,
                    state.auto_applier.as_ref(),
                    spec,
                )
                .await
                .map_err(internal)?;
            }
        }
    }

    async fn held_by(&self, id: i64, executor: &str) -> QueueResult<bool> {
        Ok(self.queue.get(id).await?.is_some_and(|r| {
            r.state == sourcegull_pgqueue::JobState::Processing && r.worker_hostname == executor
        }))
    }

    async fn heartbeat(&self, ids: &[i64], executor: &str) -> QueueResult<Heartbeat> {
        self.queue.heartbeat(ids, executor).await
    }

    async fn reset_orphans(&self, executor: &str, running: &[i64]) -> QueueResult<Vec<i64>> {
        self.queue
            .reset_orphans(executor, running, ORPHAN_GRACE)
            .await
    }

    async fn add_log(&self, id: i64, entry: &ExecutionLogEntry) -> QueueResult<Option<i32>> {
        self.queue.add_execution_log(id, entry).await
    }

    async fn update_log(
        &self,
        id: i64,
        index: i32,
        entry: &ExecutionLogEntry,
    ) -> QueueResult<bool> {
        self.queue.update_execution_log(id, index, entry).await
    }

    async fn complete(&self, id: i64, executor: &str) -> QueueResult<bool> {
        self.queue.mark_complete(id, executor).await
    }

    async fn errored(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool> {
        self.give_up(id, executor, message, true).await
    }

    async fn failed(&self, id: i64, executor: &str, message: &str) -> QueueResult<bool> {
        self.give_up(id, executor, message, false).await
    }

    async fn finished(&self, state: &AppState, id: i64) -> Result<(), DbError> {
        if let Some(spec) = self.spec_of(state, id).await? {
            sourcegull_services::batches::execution_finished(
                &state.db,
                state.auto_applier.as_ref(),
                spec,
            )
            .await?;
        }
        Ok(())
    }
}

fn new_job_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{JOB_TOKEN_PREFIX}{}", hex::encode(bytes))
}

fn unknown_queue(name: &str) -> Response {
    not_found(&format!("Unknown queue {name:?}."))
}

fn check_name(name: &str) -> Result<(), Response> {
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        Err(error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "executorName must be 1 to 255 printable characters.",
        ))
    } else {
        Ok(())
    }
}

/// `POST /.executors/queue/heartbeat`.
pub async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<HeartbeatRequest>,
) -> Response {
    if let Err(r) = check_executor_token(&state, &headers) {
        return r;
    }
    if let Err(r) = check_name(&req.executor_name) {
        return r;
    }
    let mut known = Vec::new();
    let mut cancel = Vec::new();
    let mut active = Vec::new();
    let mut queues: Vec<&str> = req.queue_names.iter().map(String::as_str).collect();
    queues.extend(req.jobs.iter().map(|j| j.queue_name.as_str()));
    queues.sort_unstable();
    queues.dedup();
    for name in queues {
        let Some(q) = queue(&state, name) else {
            continue;
        };
        let ids: Vec<i64> = req
            .jobs
            .iter()
            .filter(|j| j.queue_name == name)
            .flat_map(|j| j.job_ids.iter().copied())
            .collect();
        let hb = match q.heartbeat(&ids, &req.executor_name).await {
            Ok(hb) => hb,
            Err(e) => return internal(e),
        };
        match q.reset_orphans(&req.executor_name, &ids).await {
            Ok(reset) if !reset.is_empty() => {
                tracing::warn!(executor = %req.executor_name, queue = name, jobs = ?reset,
                    "reset jobs the executor no longer reports");
            }
            Ok(_) => {}
            Err(e) => return internal(e),
        }
        if let Some(table) = queue_table(name)
            && let Err(e) = state
                .db
                .executors()
                .delete_finished_job_tokens(name, table)
                .await
        {
            return internal(e);
        }
        active.extend(hb.known.iter().map(|&id| ActiveJob {
            queue: name.to_owned(),
            id,
        }));
        known.push(QueueJobIds {
            queue_name: name.to_owned(),
            job_ids: hb.known,
        });
        if !hb.canceled.is_empty() {
            cancel.push(QueueJobIds {
                queue_name: name.to_owned(),
                job_ids: hb.canceled,
            });
        }
    }
    let info = ExecutorInfo {
        hostname: req.executor_name.clone(),
        queue_names: req.queue_names.clone(),
        os: req.os,
        architecture: req.architecture,
        executor_version: req.version,
        runtime: req.runtime,
        runtime_version: req.runtime_version,
        git_version: req.git_version,
    };
    if let Err(e) = state.db.executors().heartbeat(&info, &active).await {
        return internal(e);
    }
    Json(HeartbeatResponse { known, cancel }).into_response()
}

/// `POST /.executors/queue/{queue}/{action}`.
pub async fn queue_action(
    State(state): State<AppState>,
    Path((name, action)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(r) = check_executor_token(&state, &headers) {
        return r;
    }
    let Some(q) = queue(&state, &name) else {
        return unknown_queue(&name);
    };
    match action.as_str() {
        "dequeue" => {
            let req: DequeueRequest = match parse(&body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Err(r) = check_name(&req.executor_name) {
                return r;
            }
            if !api::compatible(SERVER_VERSION, &req.version) {
                return error_response(
                    StatusCode::CONFLICT,
                    "VERSION_SKEW",
                    &format!(
                        "Executor version {} is not compatible with server version \
                         {SERVER_VERSION}: upgrade the executor.",
                        req.version
                    ),
                );
            }
            if !q
                .required_capabilities()
                .iter()
                .all(|c| req.capabilities.iter().any(|have| have == c))
            {
                // Jobs of this queue need steps this executor cannot run.
                return StatusCode::NO_CONTENT.into_response();
            }
            let secrets = req.capabilities.iter().any(|c| c == capability::SECRETS);
            match q.dequeue(&state, &req.executor_name, secrets).await {
                Ok(Some(job)) => Json(job).into_response(),
                Ok(None) => StatusCode::NO_CONTENT.into_response(),
                Err(r) => r,
            }
        }
        "addExecutionLogEntry" => {
            let req: AddLogEntryRequest = match parse(&body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Err(r) = held(&*q, req.job_id, &req.executor_name).await {
                return r;
            }
            match q.add_log(req.job_id, &to_entry(req.entry)).await {
                Ok(Some(index)) => Json(AddLogEntryResponse { index }).into_response(),
                Ok(None) => not_found("Unknown job."),
                Err(e) => internal(e),
            }
        }
        "updateExecutionLogEntry" => {
            let req: UpdateLogEntryRequest = match parse(&body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            if let Err(r) = held(&*q, req.job_id, &req.executor_name).await {
                return r;
            }
            match q
                .update_log(req.job_id, req.index, &to_entry(req.entry))
                .await
            {
                Ok(true) => StatusCode::NO_CONTENT.into_response(),
                Ok(false) => not_found("Unknown log entry."),
                Err(e) => internal(e),
            }
        }
        "markComplete" => {
            let req: MarkCompleteRequest = match parse(&body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            let done = q.complete(req.job_id, &req.executor_name).await;
            finish(&state, &*q, &name, req.job_id, done).await
        }
        "markErrored" | "markFailed" => {
            let req: MarkErroredRequest = match parse(&body) {
                Ok(r) => r,
                Err(r) => return r,
            };
            let message: String = req.error_message.chars().take(10_000).collect();
            let done = if action == "markErrored" {
                q.errored(req.job_id, &req.executor_name, &message).await
            } else {
                q.failed(req.job_id, &req.executor_name, &message).await
            };
            finish(&state, &*q, &name, req.job_id, done).await
        }
        _ => not_found(&format!("Unknown action {action:?}.")),
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|e| {
        error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            &format!("Invalid request body: {e}"),
        )
    })
}

async fn held(q: &dyn ExecutorQueue, id: i64, executor: &str) -> Result<(), Response> {
    match q.held_by(id, executor).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(not_found("The executor does not hold this job.")),
        Err(e) => Err(internal(e)),
    }
}

/// Ends a job: revokes its token once the transition went through, then runs the queue's
/// [`ExecutorQueue::finished`].
async fn finish(
    state: &AppState,
    q: &dyn ExecutorQueue,
    queue: &str,
    id: i64,
    done: QueueResult<bool>,
) -> Response {
    match done {
        Ok(true) => {
            if let Err(e) = state.db.executors().delete_job_token(queue, id).await {
                return internal(e);
            }
            match q.finished(state, id).await {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(e) => internal(e),
            }
        }
        Ok(false) => not_found("The executor does not hold this job."),
        Err(e) => internal(e),
    }
}

/// The running batches job behind a request's job token.
pub(crate) async fn batches_job_for(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<sourcegull_services::batches::RunningJob, Response> {
    let scope = match job_from_bearer(state, headers).await? {
        Some(scope) if scope.queue == api::BATCHES_QUEUE => scope,
        Some(_) => return Err(unauthorized("The job token is not for a batches job.")),
        None => return Err(unauthorized("A job token is required.")),
    };
    sourcegull_services::batches::running_job(&state.db, scope.job_id)
        .await
        .map_err(upload_error)
}

fn upload_error(e: sourcegull_services::batches::UploadError) -> Response {
    use sourcegull_services::batches::UploadError as E;
    match e {
        E::Invalid(m) => error_response(StatusCode::BAD_REQUEST, "INVALID_INPUT", &m),
        E::Gone(m) => not_found(m),
        E::Db(e) => internal(e),
    }
}

/// `POST /.executors/queue/batches/addCacheEntry` (job token): stores a step result.
pub async fn batches_add_cache_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let job = match batches_job_for(&state, &headers).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let req: BatchesCacheEntryRequest = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match sourcegull_services::batches::store_cache_entry(&state.db, &job, &req.key, &req.value)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => upload_error(e),
    }
}

/// `POST /.executors/queue/batches/uploadResult` (job token): stores the changeset specs.
pub async fn batches_upload_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let job = match batches_job_for(&state, &headers).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    let req: BatchesResultRequest = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match sourcegull_services::batches::store_result(&state.db, &job, &req.changeset_specs).await {
        Ok(specs) => Json(BatchesResultResponse {
            changeset_spec_ids: specs
                .iter()
                .map(|s| sourcegull_services::batches::changeset_spec_id(&s.rand_id))
                .collect(),
        })
        .into_response(),
        Err(e) => upload_error(e),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct InfoRefsParams {
    service: Option<String>,
}

/// Splits `{repo}/info/refs` or `{repo}/git-upload-pack`.
fn split_git_path<'a>(rest: &'a str, suffix: &str) -> Option<&'a str> {
    rest.strip_suffix(suffix)
        .and_then(|r| r.strip_suffix('/'))
        .map(|r| r.trim_end_matches(".git"))
        .filter(|r| !r.is_empty())
}

/// Resolves the repository of a git request and checks the job token grants it.
async fn git_reader(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
) -> Result<GitReader, Response> {
    let scope = match job_from_bearer(state, headers).await? {
        Some(scope) => scope,
        None => {
            let mut resp = unauthorized("A job token is required.");
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"sourcegull-executors\""),
            );
            return Err(resp);
        }
    };
    let repo = state
        .db
        .repos()
        .get_by_name(name)
        .await
        .map_err(internal)?
        .filter(|r| r.id == scope.repo_id)
        .ok_or_else(|| not_found("Unknown repository."))?;
    Ok(GitReader::new(state.repo_clients.clone(), &repo))
}

fn git_protocol(headers: &HeaderMap) -> String {
    headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

fn git_error(e: sourcegull_services::git::GitCallError) -> Response {
    use sourcegull_services::git::GitCallError as E;
    match e {
        E::RepoNotCloned | E::CloneInProgress => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "NOT_CLONED",
            "The repository is not cloned yet.",
        ),
        E::TooLarge(m) | E::InvalidArgument(m) => {
            error_response(StatusCode::BAD_REQUEST, "INVALID_INPUT", &m)
        }
        e => internal(e),
    }
}

fn stream_body(
    stream: sourcegull_rpc::tonic::Streaming<sourcegull_rpc::repo::v1::UploadPackResponse>,
    prefix: Option<bytes::Bytes>,
) -> Body {
    let chunks = stream.map(|r| {
        r.map(|m| m.chunk)
            .map_err(|s| std::io::Error::other(s.message().to_owned()))
    });
    match prefix {
        Some(p) => Body::from_stream(futures::stream::once(async { Ok(p) }).chain(chunks)),
        None => Body::from_stream(chunks),
    }
}

/// `GET /.executors/git/{repo}/info/refs?service=git-upload-pack`.
pub async fn git_get(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    Query(params): Query<InfoRefsParams>,
    headers: HeaderMap,
) -> Response {
    let Some(name) = split_git_path(&rest, "info/refs") else {
        return not_found("Not found.");
    };
    if params.service.as_deref() != Some("git-upload-pack") {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Only git-upload-pack (fetch) is supported.",
        );
    }
    let reader = match git_reader(&state, &headers, name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let protocol = git_protocol(&headers);
    let stream = match reader
        .upload_pack(true, bytes::Bytes::new(), &protocol)
        .await
    {
        Ok(s) => s,
        Err(e) => return git_error(e),
    };
    // Protocol v2 starts with the capability advertisement; v0 with the service line.
    let prefix = (!protocol.contains("version=2"))
        .then(|| bytes::Bytes::from_static(b"001e# service=git-upload-pack\n0000"));
    (
        [
            (
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-advertisement",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        stream_body(stream, prefix),
    )
        .into_response()
}

/// `POST /.executors/git/{repo}/git-upload-pack`.
pub async fn git_post(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(name) = split_git_path(&rest, "git-upload-pack") else {
        return not_found("Not found.");
    };
    let reader = match git_reader(&state, &headers, name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let max = MAX_GIT_REQUEST;
    let raw = match axum::body::to_bytes(body, max).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "TOO_LARGE",
                "The request is too large.",
            );
        }
    };
    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
    let request = if gzip {
        let mut out = Vec::new();
        let decoded = flate2::read::GzDecoder::new(&raw[..])
            .take(max as u64 + 1)
            .read_to_end(&mut out);
        match decoded {
            Ok(n) if n <= max => bytes::Bytes::from(out),
            _ => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "INVALID_INPUT",
                    "Invalid or too large gzip request body.",
                );
            }
        }
    } else {
        raw
    };
    let stream = match reader
        .upload_pack(false, request, &git_protocol(&headers))
        .await
    {
        Ok(s) => s,
        Err(e) => return git_error(e),
    };
    (
        [
            (header::CONTENT_TYPE, "application/x-git-upload-pack-result"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        stream_body(stream, None),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_paths() {
        assert_eq!(
            split_git_path("gitlab.com/a/b/info/refs", "info/refs"),
            Some("gitlab.com/a/b")
        );
        assert_eq!(
            split_git_path("gitlab.com/a/b.git/git-upload-pack", "git-upload-pack"),
            Some("gitlab.com/a/b")
        );
        assert_eq!(split_git_path("info/refs", "info/refs"), None);
        assert_eq!(split_git_path("a/b/x", "info/refs"), None);
    }

    #[test]
    fn index_job_steps() {
        let job = IndexJob {
            repo_id: sourcegull_types::RepoId(1),
            commit: "a".repeat(40),
            root: "web/".into(),
            indexer: "sourcegraph/scip-typescript:latest".into(),
            indexer_args: vec!["scip-typescript".into(), "index".into()],
            outfile: String::new(),
            docker_steps: vec![sourcegull_codeintel_scip::store::IndexDockerStep {
                root: "web/".into(),
                image: "node:20".into(),
                commands: vec!["npm ci".into()],
            }],
            local_steps: vec!["echo hi".into()],
            requested_env_vars: Vec::new(),
            should_reindex: false,
            enqueuer: None,
            associated_upload_id: None,
        };
        let j = index_job_to_job(7, "gitlab.com/a/b", &job, "sjt_x");
        assert_eq!(j.docker_steps.len(), 2);
        assert_eq!(j.docker_steps[0].key, "step.docker.pre-index.0");
        assert_eq!(j.docker_steps[0].dir, "web/");
        assert_eq!(
            j.docker_steps[1].commands,
            vec!["echo hi".to_owned(), "scip-typescript index".to_owned()]
        );
        let upload = j.upload.unwrap();
        assert_eq!(
            (upload.file.as_str(), upload.root.as_str()),
            ("web/index.scip", "web/")
        );
        assert_eq!(j.redacted_values["sjt_x"], "JOB_TOKEN_REMOVED");
    }
}
