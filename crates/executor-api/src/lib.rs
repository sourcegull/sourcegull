//! The executor queue protocol (`/.executors/queue/…`).
//!
//! Executors call the web role over HTTP. Queue calls authenticate with the shared executor
//! token (`Authorization: token-executor <token>`, site config `executors.accessToken`).
//! Each dequeued [`Job`] carries a job token, sent as `Authorization: Bearer <token>`, that
//! is good for that job's repository only: the git checkout (`/.executors/git/{repo}`) and
//! the SCIP upload (`/.api/scip/upload`). It is revoked when the job finishes.
//!
//! | Call | Request | Response |
//! |---|---|---|
//! | `POST /.executors/queue/{queue}/dequeue` | [`DequeueRequest`] | 200 [`Job`], or 204 when the queue is empty |
//! | `POST /.executors/queue/heartbeat` | [`HeartbeatRequest`] | [`HeartbeatResponse`] |
//! | `POST /.executors/queue/{queue}/addExecutionLogEntry` | [`AddLogEntryRequest`] | [`AddLogEntryResponse`] |
//! | `POST /.executors/queue/{queue}/updateExecutionLogEntry` | [`UpdateLogEntryRequest`] | 204 |
//! | `POST /.executors/queue/{queue}/markComplete` | [`MarkCompleteRequest`] | 204 |
//! | `POST /.executors/queue/{queue}/markErrored` | [`MarkErroredRequest`] | 204 |
//! | `POST /.executors/queue/{queue}/markFailed` | [`MarkErroredRequest`] | 204 |
//!
//! A job the executor no longer holds (reset, canceled, or held by another executor) answers
//! 404 on the per-job calls; the executor then abandons it.
//!
//! # The `batches` queue
//!
//! Workspace executions of server-side bulk change specs. A [`Job`] of this queue has no `dockerSteps` and no `upload`; its [`Job::batches`] ([`BatchesJob`]) is what
//! the executor needs to build a `sourcegull_batches_steps::Task` and run it with
//! `sourcegull_batches_steps::StepRunner`. Executors ask for these jobs with the
//! [`capability::BATCH_STEPS`] and [`capability::SECRETS`] capabilities. Jobs are handed out
//! fairly between users (the user whose last dequeue is the oldest goes first).
//!
//! What the executor does with a batches job:
//!
//! 1. Checks out [`Job::commit`] of [`Job::repository_name`] from `/.executors/git/…` with
//!    the job token (`shallowClone`; `sparseCheckout` holds the workspace path when the
//!    workspace sets `onlyFetchWorkspace`).
//! 2. Parses [`BatchesJob::raw_spec`] with `sourcegull_batches::BatchSpec::parse` and builds the
//!    `Task` from [`BatchesJob::repository`], [`BatchesJob::path`] and
//!    [`BatchesJob::only_fetch_workspace`]. [`Job::secret_env`] (`NAME=value`, the
//!    namespace's `BATCHES` executor secrets the steps pass through) becomes
//!    `Task::global_env`: unlike docker steps, a value only reaches the steps that list
//!    its name in `env`. Values are in [`Job::redacted_values`] as usual.
//! 3. Runs the steps with a step cache that answers lookups from
//!    [`BatchesJob::cached_steps`] (none with [`BatchesJob::no_cache`]) and stores each new
//!    step result with [`BATCHES_ADD_CACHE_ENTRY`] (also with `noCache`, so later runs can use
//!    it). The server keeps entries per user in `batch_spec_execution_cache_entries`.
//! 4. Reports progress in the execution log (`addExecutionLogEntry`/`updateExecutionLogEntry`,
//!    redacted), with these keys, which the server turns into the workspace's `stages` and
//!    `steps` (steps numbered from 0):
//!    - `setup.<name>`: checkout and image pulls (`stages.setup`);
//!    - `step.<i>`: step `i` ran: `command` is `[container, rendered run]`, `out` its output
//!      with each line prefixed `stdout: ` or `stderr: `, `exitCode`, `startTime` and
//!      `durationMs` once it finished (`stages.srcExec` and `steps[i]`);
//!    - `step.<i>.skipped`: step `i`'s `if:` was false;
//!    - `step.<i>.cached`: steps `0..=i` were restored from the cache (the run resumed after
//!      step `i`);
//!    - `teardown.<name>`: cleanup (`stages.teardown`).
//! 5. Builds the changeset specs with `sourcegull_batches_steps::build_changeset_specs`
//!    ([`BatchesRepository`] is its `SpecRepository`, [`BatchesJob::fallback_author`] its
//!    fallback author) and uploads them with [`BATCHES_UPLOAD_RESULT`] (an empty list when the
//!    diff is empty), then calls `markComplete`.
//! 6. On a failure: `markFailed` for step failures (a non-zero exit, a template error: running
//!    again will not help), `markErrored` for infrastructure trouble (retried).
//!
//! Cancellation: when a user cancels the execution, the next heartbeat lists the job in
//! [`HeartbeatResponse::cancel`]; the executor stops it and calls `markFailed` (or
//! `markErrored`), which the server records as `canceled`.
//!
//! The batches calls that carry results authenticate with the job token
//! (`Authorization: Bearer <token>`) instead of the executor token; the token names the job:
//!
//! | Call | Request | Response |
//! |---|---|---|
//! | `POST /.executors/queue/batches/addCacheEntry` | [`BatchesCacheEntryRequest`] | 204 |
//! | `POST /.executors/queue/batches/uploadResult` | [`BatchesResultRequest`] | 200 [`BatchesResultResponse`] |
//! | `GET /.executors/files/batch-changes/{spec}/{file}` | — | 200, the file ([`BatchesWorkspaceFile::url`]) |
//!
//! The uploads answer 404 once the job is no longer processing, 400 for an entry whose key is
//! not one of the job's step cache keys or a changeset spec that is invalid (as
//! `createChangesetSpec` validates) or names another repository. The download answers 403 for
//! another spec's file and 401 once the job has finished (its token is revoked).
//!
//! Files for `mount`: a server-side spec's steps mount files that were uploaded with the spec
//! (to `POST /.api/files/batch-changes/{spec}`,
//! stored in `batch_spec_workspace_files`). [`BatchesJob::workspace_files`] lists them; the
//! executor downloads each one from [`BatchesWorkspaceFile::url`] (a
//! `GET /.executors/files/batch-changes/{spec}/{file}` path, with the job token as
//! `Authorization: Bearer <token>`), writes it at [`BatchesWorkspaceFile::path`] under a
//! directory of its own, and runs the steps with that directory as the mount root
//! (`StepRunner::with_mount_root`), so `mount: [{path: ./scripts/x.sh, …}]` resolves as it does
//! next to the spec on the uploader's machine. Mount paths outside that directory fail the step.
//! Executors that predate the field ignore it, and steps with `mount` then fail.

pub mod secrets;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The `Authorization` scheme of the shared executor token.
pub const EXECUTOR_TOKEN_SCHEME: &str = "token-executor";

/// The code intelligence (auto-indexing) queue.
pub const CODEINTEL_QUEUE: &str = "codeintel";

/// The bulk changes workspace execution queue.
pub const BATCHES_QUEUE: &str = "batches";

/// Path of the step cache upload ([`BatchesCacheEntryRequest`]).
pub const BATCHES_ADD_CACHE_ENTRY: &str = "/.executors/queue/batches/addCacheEntry";

/// Path of the changeset spec upload ([`BatchesResultRequest`]).
pub const BATCHES_UPLOAD_RESULT: &str = "/.executors/queue/batches/uploadResult";

/// The [`BatchesJob`] payload format this protocol version describes.
pub const BATCHES_JOB_VERSION: i32 = 2;

/// Capabilities this protocol version understands. The server only hands out jobs whose
/// steps the executor can run.
pub mod capability {
    /// Runs [`DockerStep`](super::DockerStep)s.
    pub const DOCKER_STEPS: &str = "docker-steps";
    /// Uploads SCIP indexes ([`UploadStep`](super::UploadStep)).
    pub const SCIP_UPLOAD: &str = "scip-upload";
    /// Injects executor secrets ([`Job::secret_env`](super::Job::secret_env)) and pulls with
    /// [`Job::docker_auth_config`](super::Job::docker_auth_config). Executors without it only
    /// get jobs that request no secrets.
    pub const SECRETS: &str = "executor-secrets";
    /// Runs bulk change spec steps with `sourcegull-batches-steps` ([`BatchesJob`](super::BatchesJob),
    /// payload version [`BATCHES_JOB_VERSION`](super::BATCHES_JOB_VERSION)).
    pub const BATCH_STEPS: &str = "batch-steps";
}

/// Asks for one job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DequeueRequest {
    /// Unique executor name (the worker id of the jobs it holds).
    pub executor_name: String,
    /// The executor's version (`major.minor.patch`); it may lag the server by one minor
    /// version.
    pub version: String,
    /// Step kinds the executor can run (see [`capability`]).
    pub capabilities: Vec<String>,
}

/// A job to run: check out `commit` of `repository_name` into the workspace, run the docker
/// steps in order, then the upload step if any.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Job {
    pub id: i64,
    pub queue: String,
    /// Job-scoped token for checkout and upload.
    pub token: String,
    pub repository_name: String,
    pub commit: String,
    /// Fetch only `commit` with `--depth=1`.
    pub shallow_clone: bool,
    /// Also fetch tags.
    pub fetch_tags: bool,
    /// Tags pointing at `commit` (annotated tags peeled), created locally as lightweight tags
    /// after the fetch so tools that read the version from a tag on `HEAD` see it. Runtimes
    /// skip names that fail [`valid_tag_name`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commit_tags: Vec<String>,
    /// Check out only these directories (cone mode); everything when empty.
    pub sparse_checkout: Vec<String>,
    pub docker_steps: Vec<DockerStep>,
    pub upload: Option<UploadStep>,
    /// Values replaced in every log line (value → replacement), e.g. secrets in env vars.
    pub redacted_values: BTreeMap<String, String>,
    /// Executor secrets as `NAME=value`, added to the environment of every docker
    /// step. Their values are also in `redacted_values`. Runtimes keep them out of anything
    /// they log or persist in plain text (the Kubernetes runtime passes them through the job's
    /// Secret).
    pub secret_env: Vec<String>,
    /// Registry credentials for pulling the steps' images, from the `DOCKER_AUTH_CONFIG`
    /// executor secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_auth_config: Option<DockerAuthConfig>,
    /// A bulk change spec workspace execution (`batches` queue only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batches: Option<BatchesJob>,
}

/// One workspace execution of a bulk change spec: what `sourcegull_batches_steps::Task` and
/// `build_changeset_specs` need (see the module docs, "The `batches` queue").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesJob {
    /// [`BATCHES_JOB_VERSION`].
    pub version: i32,
    /// The bulk change spec's GraphQL ID.
    pub batch_spec_id: String,
    /// The workspace's GraphQL ID.
    pub workspace_id: String,
    /// The bulk change spec as the user wrote it (YAML or JSON); parse it with
    /// `sourcegull_batches::BatchSpec::parse`. Its `name` and `description` are
    /// `batch_change.*`, its `steps` run, its `changesetTemplate` and `transformChanges`
    /// shape the changeset specs.
    pub raw_spec: String,
    pub repository: BatchesRepository,
    /// The workspace directory relative to the repository root (`steps.path`); '' for the
    /// root.
    pub path: String,
    /// `workspaces[].onlyFetchWorkspace` (part of the step cache keys).
    pub only_fetch_workspace: bool,
    /// The spec was executed with `noCache`: run every step ([`Self::cached_steps`] is
    /// empty), still upload the results.
    pub no_cache: bool,
    /// Step results found in the cache when the workspace was resolved, first step first.
    /// The step cache serves lookups from these; the runner still checks each entry's
    /// fingerprint (image digests, secret values, mounts) and runs the steps again when it
    /// differs.
    pub cached_steps: Vec<BatchesCachedStep>,
    /// The commit author when `changesetTemplate.commit.author` is unset: the user who
    /// created the spec (name and primary email), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_author: Option<BatchesCommitAuthor>,
    /// Files the steps may `mount`, uploaded with the spec (see the module docs, "Files for
    /// `mount`"). Empty when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspace_files: Vec<BatchesWorkspaceFile>,
}

/// A file uploaded with a server-side bulk change spec, for steps that `mount` it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesWorkspaceFile {
    /// The file's path relative to the bulk change spec's directory (`scripts/x.sh`), always with
    /// `/` and never leaving that directory.
    pub path: String,
    /// Where to download it: a path on the instance
    /// (`/.executors/files/batch-changes/{spec}/{file}`), authenticated with the job token.
    pub url: String,
    /// Size in bytes.
    pub size: i64,
    pub modified_at: DateTime<Utc>,
}

/// The repository of a workspace (`sourcegull_batches_steps::TaskRepository` and
/// `SpecRepository`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesRepository {
    /// GraphQL ID (`baseRepository`/`headRepository` of the changeset specs).
    pub id: String,
    /// `gitlab.example.com/group/project` (`repository.name`), as [`Job::repository_name`].
    pub name: String,
    /// The branch the workspace was resolved on, `refs/heads/…` (changeset specs' `baseRef`).
    pub base_ref: String,
    /// The same branch without `refs/heads/` (`repository.branch`).
    pub branch: String,
    /// The commit to run on, as [`Job::commit`] (changeset specs' `baseRev`).
    pub base_rev: String,
    /// Paths of the `on` query's results in the workspace (`repository.search_result_paths`).
    pub file_matches: Vec<String>,
}

/// One cached step result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesCachedStep {
    /// The step, from 0.
    pub step_index: usize,
    /// The step's cache key (`sourcegull_batches_steps::StepCacheKey::key`).
    pub key: String,
    /// The entry: a `sourcegull_batches_steps::CachedStep` as JSON.
    pub value: String,
}

/// A commit author.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchesCommitAuthor {
    pub name: String,
    pub email: String,
}

/// Stores one step result in the execution cache of the job's user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesCacheEntryRequest {
    /// One of the job's step cache keys (`sourcegull_batches_steps::StepCacheKey::key`).
    pub key: String,
    /// A `sourcegull_batches_steps::CachedStep` as JSON, whose result is of the key's step.
    pub value: String,
}

/// The changeset specs a workspace execution produced; replaces any uploaded earlier for the
/// job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesResultRequest {
    /// Changeset specs as JSON (what `createChangesetSpec` accepts), all of the job's
    /// repository.
    pub changeset_specs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BatchesResultResponse {
    /// GraphQL IDs of the stored changeset specs, in upload order.
    pub changeset_spec_ids: Vec<String>,
}

/// The part of a Docker CLI `config.json` that authenticates to registries:
/// `{"auths": {"registry.example.com": {"auth": "base64(user:password)"}}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerAuthConfig {
    /// Registry host (or URL, e.g. `https://index.docker.io/v1/`) → credentials.
    pub auths: BTreeMap<String, DockerAuthConfigAuth>,
}

/// One registry's credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerAuthConfigAuth {
    /// Base64 of `username:password`.
    pub auth: String,
}

/// A step run in a container with the workspace mounted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DockerStep {
    /// Log key, e.g. `step.docker.indexer`.
    pub key: String,
    pub image: String,
    /// Shell commands, run in order by `/bin/sh` with `set -e`.
    pub commands: Vec<String>,
    /// Working directory relative to the repository root ('' for the root).
    pub dir: String,
    /// `NAME=value` environment entries.
    pub env: Vec<String>,
}

/// Uploads a SCIP index from the workspace with the job token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UploadStep {
    pub key: String,
    /// The index file, relative to the repository root.
    pub file: String,
    /// The upload's root directory ('' or `dir/`).
    pub root: String,
}

/// Jobs one executor is running in one queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct QueueJobIds {
    pub queue_name: String,
    pub job_ids: Vec<i64>,
}

/// Sent every few seconds: refreshes the jobs' heartbeats and the executor's admin row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatRequest {
    pub executor_name: String,
    pub queue_names: Vec<String>,
    pub jobs: Vec<QueueJobIds>,
    pub os: String,
    pub architecture: String,
    pub version: String,
    /// `docker` or `kubernetes`.
    pub runtime: String,
    pub runtime_version: String,
    pub git_version: String,
}

/// Which reported jobs the executor still holds, and which of them to stop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatResponse {
    /// Jobs still held; any other reported job was taken away and must be abandoned.
    pub known: Vec<QueueJobIds>,
    /// Jobs whose cancellation was requested.
    pub cancel: Vec<QueueJobIds>,
}

/// One entry of a job's execution log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogEntry {
    pub key: String,
    pub command: Vec<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub out: String,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AddLogEntryRequest {
    pub executor_name: String,
    pub job_id: i64,
    pub entry: LogEntry,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AddLogEntryResponse {
    /// The entry's index, for [`UpdateLogEntryRequest`].
    pub index: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateLogEntryRequest {
    pub executor_name: String,
    pub job_id: i64,
    pub index: i32,
    pub entry: LogEntry,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarkCompleteRequest {
    pub executor_name: String,
    pub job_id: i64,
}

/// `markErrored` (retried with backoff while attempts remain) and `markFailed` (final).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MarkErroredRequest {
    pub executor_name: String,
    pub job_id: i64,
    pub error_message: String,
}

/// `major.minor` of a version string, if it has them.
pub fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim_start_matches('v').split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Whether an executor at `executor` may talk to a server at `server`: same major version and
/// at most one minor version behind (or ahead, during a rollout). Unparseable versions (dev
/// builds) are accepted.
pub fn compatible(server: &str, executor: &str) -> bool {
    match (major_minor(server), major_minor(executor)) {
        (Some((smaj, smin)), Some((emaj, emin))) => smaj == emaj && smin.abs_diff(emin) <= 1,
        _ => true,
    }
}

/// Whether `name` is a tag name `git tag <name>` accepts: `refs/tags/<name>` passes the
/// `git check-ref-format` rules and `name` does not start with `-`.
pub fn valid_tag_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name == "@"
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
    {
        return false;
    }
    if name.chars().any(|c| {
        c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    name.split('/')
        .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_skew() {
        assert!(compatible("0.1.0", "0.1.3"));
        assert!(compatible("0.2.0", "0.1.9"));
        assert!(!compatible("0.3.0", "0.1.0"));
        assert!(!compatible("1.0.0", "0.9.0"));
        assert!(compatible("0.3.0", "dev"));
        assert_eq!(major_minor("v5.2.1-rc1"), Some((5, 2)));
    }

    #[test]
    fn job_wire_format() {
        let job: Job = serde_json::from_str(
            r#"{"id": 3, "queue": "codeintel", "repositoryName": "a/b", "dockerSteps": [{"image": "x", "commands": ["true"]}]}"#,
        )
        .unwrap();
        assert_eq!(job.docker_steps[0].image, "x");
        assert!(job.upload.is_none());
        assert!(job.commit_tags.is_empty());
    }

    #[test]
    fn commit_tags_wire_format() {
        // A server that sends no tags (or an older one) and an executor that knows none.
        let json = serde_json::to_value(Job::default()).unwrap();
        assert!(json.get("commitTags").is_none());
        let job = Job {
            commit_tags: vec!["v1.2.0".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(json["commitTags"], serde_json::json!(["v1.2.0"]));
        assert_eq!(serde_json::from_value::<Job>(json).unwrap(), job);
    }

    #[test]
    fn tag_names() {
        for ok in [
            "v1.0",
            "v1.2.3-rc.1",
            "release/2024",
            "pkg/batch-kafka/v1.1.2",
            "a@b",
        ] {
            assert!(valid_tag_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "/a",
            "a/",
            "a.",
            "@",
            "a..b",
            "a//b",
            "a@{1}",
            ".a",
            "a/.b",
            "a.lock",
            "a/b.lock/c",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "a\nb",
        ] {
            assert!(!valid_tag_name(bad), "{bad:?}");
        }
        // Valid for git; runtimes never put names in a shell string.
        assert!(valid_tag_name("$(id)"));
    }
}
