//! Jobs of the `batches` queue: one bulk change spec workspace, run with
//! the batch step runner (`sourcegull-batches-steps`) as the contract in [`sourcegull_executor_api`]
//! ("The `batches` queue") describes.
//!
//! [`run_job`] checks the workspace out (sparse when the workspace sets `onlyFetchWorkspace`),
//! downloads the spec's workspace files (for `mount`, [`download_files`]) into a directory
//! of their own that `mount` paths are resolved in,
//! builds the [`Task`] from the [`BatchesJob`] payload (executor secrets become
//! [`Task::global_env`]), runs the steps with [`StepRunner`] on a [`ContainerRuntime`], and
//! uploads the changeset specs of the final result. Step results come from and go to the
//! server's execution cache through [`JobStepCache`]; the runner's events become execution
//! log entries (`setup.*`, `step.<i>`, `step.<i>.skipped`, `step.<i>.cached`, `teardown.*`),
//! redacted by the job's logger.
//!
//! The job runner is generic over the container runtime: [`crate::docker::DockerRuntime`]
//! runs it with the step runner's Docker runtime, [`crate::kubernetes::KubernetesRuntime`]
//! with [`crate::kubernetes::steps::KubernetesSteps`], [`StepsRuntime`] with any other (the
//! in-process fake in tests).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sourcegull_batches::BatchSpec;
use sourcegull_batches::spec::CommitAuthor;
use sourcegull_batches_steps::cache::CacheError;
use sourcegull_batches_steps::{
    CachedStep, ChangesetSpecInput, ContainerRuntime, LogSink, OutputStream, RuntimeError,
    SpecRepository, StepCache, StepEvent, StepRunner, StepsError, Task, TaskRepository,
    build_changeset_specs,
};
use sourcegull_executor_api::{self as api, BatchesJob, Job, capability};
use tokio::sync::mpsc;

use crate::checkout::checkout;
use crate::client::{Client, ClientError};
use crate::logger::{JobLogger, Redactor, StepLog};
use crate::{JobContext, JobError, Runtime};

/// Where the repository is checked out in the job's scratch directory.
const WORKSPACE_DIR: &str = "repository";
/// Where the spec's workspace files are downloaded in the job's scratch directory: the
/// runner's mount root.
const FILES_DIR: &str = "workspace-files";

/// Runs one `batches` job with `runtime`: checkout, steps, changeset spec upload.
pub async fn run_job(
    job: &Job,
    ctx: &JobContext,
    runtime: Arc<dyn ContainerRuntime>,
) -> Result<(), JobError> {
    let payload = job
        .batches
        .as_ref()
        .ok_or_else(|| JobError::Failed("the job has no bulk change spec workspace".into()))?;
    if payload.version != api::BATCHES_JOB_VERSION {
        return Err(JobError::Failed(format!(
            "the batches job has payload version {}, this executor runs version {}",
            payload.version,
            api::BATCHES_JOB_VERSION
        )));
    }
    let spec = BatchSpec::parse(&payload.raw_spec)
        .map_err(|e| JobError::Failed(format!("the bulk change spec is not valid: {e}")))?;

    let workspace = ctx.scratch.join(WORKSPACE_DIR);
    checkout(
        &ctx.git,
        &ctx.client,
        job,
        &workspace,
        &ctx.logger,
        &ctx.cancel,
    )
    .await?;
    let files = ctx.scratch.join(FILES_DIR);
    download_files(job, ctx, &payload.workspace_files, &files).await?;

    let task = task(spec, payload, &job.secret_env);
    let cache = Arc::new(JobStepCache::new(ctx.client.clone(), job));
    let (events, rx) = mpsc::unbounded_channel();
    let logging = tokio::spawn(log_events(ctx.logger.clone(), rx));
    let outcome = StepRunner::new(runtime)
        .with_cache(cache)
        .with_log(Arc::new(ChannelSink(events)))
        .with_cancel(ctx.cancel.clone())
        .with_mount_root(&files)
        .run(&task, &workspace)
        .await;
    // The runner (and with it the sink's sender) is gone: the logger drains and ends.
    let logged = logging
        .await
        .map_err(|e| JobError::Errored(format!("logging the steps: {e}")))?;
    if let Err(ClientError::NotHeld) = logged {
        return Err(ClientError::NotHeld.into());
    }
    let outcome = outcome.map_err(steps_error)?;
    logged?;

    upload_result(job, ctx, &task, payload, &outcome.result).await
}

/// Downloads the spec's workspace files with the job token to their paths under `dir`
/// (`setup.workspace-files`, when there are any). `dir` is created even without files.
pub async fn download_files(
    job: &Job,
    ctx: &JobContext,
    files: &[api::BatchesWorkspaceFile],
    dir: &std::path::Path,
) -> Result<(), JobError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| JobError::Errored(format!("creating the workspace files directory: {e}")))?;
    if files.is_empty() {
        return Ok(());
    }
    let log = ctx
        .logger
        .start(
            "setup.workspace-files",
            &["download".to_owned(), "workspace files".to_owned()],
        )
        .await?;
    let outcome: Result<(), JobError> = async {
        for f in files {
            let rel = workspace_file_path(&f.path).ok_or_else(|| {
                JobError::Failed(format!("the workspace file path {:?} is not valid", f.path))
            })?;
            let dest = dir.join(&rel);
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| JobError::Errored(format!("writing {}: {e}", f.path)))?;
            }
            let download = ctx.client.batches_download_file(job, &f.url, &dest);
            tokio::select! {
                r = download => r.map_err(|e| match e {
                    ClientError::NotHeld => JobError::Client(ClientError::NotHeld),
                    e => JobError::Errored(format!("downloading {}: {e}", f.path)),
                })?,
                _ = ctx.cancel.cancelled() => return Err(JobError::Canceled),
            }
            log.line(&format!("{} ({} bytes)", f.path, f.size));
        }
        Ok(())
    }
    .await;
    match &outcome {
        Ok(()) => log.finish(Some(0)).await?,
        Err(JobError::Canceled) => log.finish(None).await?,
        Err(e) => {
            log.line(&e.to_string());
            log.finish(Some(1)).await?;
        }
    }
    outcome
}

/// A workspace file's path as a relative path that stays in its directory.
fn workspace_file_path(path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    let ok = !path.is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    ok.then(|| p.to_owned())
}

/// The step runner's task for a payload.
fn task(spec: BatchSpec, payload: &BatchesJob, secret_env: &[String]) -> Task {
    let repo = &payload.repository;
    Task {
        spec,
        repository: TaskRepository {
            name: repo.name.clone(),
            branch: repo.branch.clone(),
            base_rev: repo.base_rev.clone(),
            file_matches: repo.file_matches.clone(),
        },
        path: payload.path.clone(),
        only_fetch_workspace: payload.only_fetch_workspace,
        global_env: global_env(secret_env),
    }
}

/// The values of pass-through variables: the job's executor secrets (`NAME=value`).
pub fn global_env(secret_env: &[String]) -> HashMap<String, String> {
    secret_env
        .iter()
        .filter_map(|e| e.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// A step runner error as a job outcome: what running again cannot fix fails the job (a
/// step's non-zero exit, templates, outputs, paths, images that cannot be pulled), the rest
/// errors it so it is retried.
fn steps_error(e: StepsError) -> JobError {
    match e {
        StepsError::Canceled
        | StepsError::Runtime {
            error: RuntimeError::Canceled,
            ..
        } => JobError::Canceled,
        StepsError::Template { .. }
        | StepsError::Output { .. }
        | StepsError::StepFailed { .. }
        | StepsError::InvalidPath { .. }
        | StepsError::ChangesetTemplate { .. }
        | StepsError::TransformChanges(_)
        | StepsError::Image(RuntimeError::Image { .. })
        | StepsError::Runtime {
            error: RuntimeError::Image { .. },
            ..
        } => JobError::Failed(e.to_string()),
        StepsError::Image(_)
        | StepsError::Runtime { .. }
        | StepsError::Git(_)
        | StepsError::Io(_) => JobError::Errored(e.to_string()),
    }
}

/// Builds the changeset specs of the final result and uploads them (`teardown.upload`).
async fn upload_result(
    job: &Job,
    ctx: &JobContext,
    task: &Task,
    payload: &BatchesJob,
    result: &sourcegull_batches_steps::AfterStepResult,
) -> Result<(), JobError> {
    let log = ctx
        .logger
        .start(
            "teardown.upload",
            &["upload".to_owned(), "changeset specs".to_owned()],
        )
        .await?;
    let outcome: Result<(), JobError> = async {
        let repo = &payload.repository;
        let repository = SpecRepository {
            id: repo.id.clone(),
            name: repo.name.clone(),
            base_ref: repo.base_ref.clone(),
            base_rev: repo.base_rev.clone(),
            file_matches: repo.file_matches.clone(),
        };
        let author = payload.fallback_author.as_ref().map(|a| CommitAuthor {
            name: a.name.clone(),
            email: a.email.clone(),
        });
        let specs = build_changeset_specs(&ChangesetSpecInput {
            spec: &task.spec,
            repository: &repository,
            path: &task.path,
            result,
            fallback_author: author.as_ref(),
        })
        .map_err(|e| JobError::Failed(e.to_string()))?;
        log.line(&format!("uploading {} changeset spec(s)", specs.len()));
        let resp = ctx
            .client
            .batches_upload_result(job, specs.iter().map(|s| s.to_json()).collect())
            .await
            .map_err(|e| match e {
                ClientError::NotHeld => JobError::Client(ClientError::NotHeld),
                // The server refused the specs: running again builds the same ones.
                e @ ClientError::Status { status, .. } if status.is_client_error() => {
                    JobError::Failed(format!("uploading the changeset specs: {e}"))
                }
                e => JobError::Errored(format!("uploading the changeset specs: {e}")),
            })?;
        for id in &resp.changeset_spec_ids {
            log.line(&format!("created changeset spec {id}"));
        }
        Ok(())
    }
    .await;
    match &outcome {
        Ok(()) => log.finish(Some(0)).await?,
        Err(e) => {
            log.line(&e.to_string());
            log.finish(Some(1)).await?;
        }
    }
    outcome
}

/// The step cache of a job: lookups are answered from the payload's cached steps (what the
/// resolver found in the user's execution cache), new results are stored on the server with
/// `addCacheEntry` (also with `noCache`, so later executions can use them). Step output in
/// stored results is redacted like the job's logs.
pub struct JobStepCache {
    client: Client,
    job: Job,
    redactor: Redactor,
    entries: Mutex<HashMap<String, String>>,
}

impl JobStepCache {
    pub fn new(client: Client, job: &Job) -> Self {
        let entries = job
            .batches
            .iter()
            .flat_map(|b| &b.cached_steps)
            .map(|s| (s.key.clone(), s.value.clone()))
            .collect();
        Self {
            client,
            job: job.clone(),
            redactor: Redactor::for_job(job),
            entries: Mutex::new(entries),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl StepCache for JobStepCache {
    async fn get(&self, key: &str) -> Result<Option<CachedStep>, CacheError> {
        let value = self.lock().get(key).cloned();
        value
            .map(|v| CachedStep::from_json(&v).map_err(|e| CacheError(e.to_string())))
            .transpose()
    }

    async fn set(&self, key: &str, entry: &CachedStep) -> Result<(), CacheError> {
        let mut entry = entry.clone();
        entry.result.stdout = self.redactor.redact(&entry.result.stdout);
        entry.result.stderr = self.redactor.redact(&entry.result.stderr);
        let value = entry.to_json();
        self.client
            .batches_add_cache_entry(&self.job, key, &value)
            .await
            .map_err(|e| CacheError(e.to_string()))?;
        self.lock().insert(key.to_owned(), value);
        Ok(())
    }
}

/// A runner event, owned so it can cross to the logging task.
#[derive(Debug)]
enum Event {
    ImageResolved {
        image: String,
        digest: String,
    },
    CacheHit(usize),
    Skipped(usize),
    Started {
        step: usize,
        image: String,
        script: String,
    },
    Output {
        stream: OutputStream,
        data: Vec<u8>,
    },
    Finished {
        exit_code: i32,
    },
    Warning(String),
}

/// Forwards the runner's (synchronous) events to [`log_events`].
struct ChannelSink(mpsc::UnboundedSender<Event>);

impl LogSink for ChannelSink {
    fn event(&self, event: &StepEvent<'_>) {
        let event = match *event {
            StepEvent::ImageResolved { image, digest } => Event::ImageResolved {
                image: image.to_owned(),
                digest: digest.to_owned(),
            },
            StepEvent::CacheHit { step } => Event::CacheHit(step),
            StepEvent::Skipped { step } => Event::Skipped(step),
            StepEvent::Started {
                step,
                image,
                script,
                ..
            } => Event::Started {
                step,
                image: image.to_owned(),
                script: script.to_owned(),
            },
            StepEvent::Output { stream, data, .. } => Event::Output {
                stream,
                data: data.to_vec(),
            },
            StepEvent::Finished { exit_code, .. } => Event::Finished { exit_code },
            StepEvent::Warning { message } => Event::Warning(message.to_owned()),
        };
        // The receiver only goes away once the run is over.
        let _ = self.0.send(event);
    }
}

/// The log entry of the running step, with the unfinished line of each stream.
struct OpenStep {
    log: StepLog,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl OpenStep {
    /// Appends `data` and logs each completed line as `stdout: …` / `stderr: …`.
    fn output(&mut self, stream: OutputStream, data: &[u8]) {
        let (buf, prefix) = match stream {
            OutputStream::Stdout => (&mut self.stdout, "stdout"),
            OutputStream::Stderr => (&mut self.stderr, "stderr"),
        };
        buf.extend_from_slice(data);
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            return;
        };
        let complete: Vec<u8> = buf.drain(..=end).collect();
        let mut text = String::new();
        for line in String::from_utf8_lossy(&complete[..end]).split('\n') {
            text.push_str(prefix);
            text.push_str(": ");
            text.push_str(line.strip_suffix('\r').unwrap_or(line));
            text.push('\n');
        }
        self.log.append(&text);
    }

    /// Logs what is left of unfinished lines and ends the entry.
    async fn finish(mut self, exit_code: Option<i32>) -> Result<(), ClientError> {
        if !self.stdout.is_empty() {
            self.output(OutputStream::Stdout, b"\n");
        }
        if !self.stderr.is_empty() {
            self.output(OutputStream::Stderr, b"\n");
        }
        self.log.finish(exit_code).await
    }
}

/// Turns runner events into execution log entries. Ends when the runner is gone; returns the
/// first logging error (later events are dropped after one).
async fn log_events(
    logger: JobLogger,
    mut events: mpsc::UnboundedReceiver<Event>,
) -> Result<(), ClientError> {
    let mut open: Option<OpenStep> = None;
    let mut failed: Option<ClientError> = None;
    while let Some(event) = events.recv().await {
        if failed.is_some() {
            continue;
        }
        let result = match event {
            Event::ImageResolved { image, digest } => {
                logger
                    .note(
                        &format!("setup.image.{image}"),
                        &format!("{image} resolved to {digest}"),
                        0,
                    )
                    .await
            }
            Event::CacheHit(step) => {
                logger
                    .note(
                        &format!("step.{step}.cached"),
                        &format!(
                            "restored the result of steps 1 to {} from the cache",
                            step + 1
                        ),
                        0,
                    )
                    .await
            }
            Event::Skipped(step) => {
                logger
                    .note(
                        &format!("step.{step}.skipped"),
                        &format!("step {} skipped: its if: condition is false", step + 1),
                        0,
                    )
                    .await
            }
            Event::Started {
                step,
                image,
                script,
            } => match logger
                .start(&format!("step.{step}"), &[image, script])
                .await
            {
                Ok(log) => {
                    open = Some(OpenStep {
                        log,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Event::Output { stream, data } => {
                if let Some(step) = open.as_mut() {
                    step.output(stream, &data);
                }
                Ok(())
            }
            Event::Finished { exit_code } => match open.take() {
                Some(step) => step.finish(Some(exit_code)).await,
                None => Ok(()),
            },
            Event::Warning(message) => {
                tracing::warn!(message, "batch step runner warning");
                if let Some(step) = open.as_ref() {
                    step.log.line(&format!("warning: {message}"));
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            failed = Some(e);
        }
    }
    // A step that did not finish (canceled, runtime error).
    if let Some(step) = open.take()
        && failed.is_none()
    {
        step.log.line("the step did not finish");
        if let Err(e) = step.finish(None).await {
            failed = Some(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

/// A [`Runtime`] that runs only `batches` jobs, on any [`ContainerRuntime`] (the step runner's
/// in-process fake in tests, other container runtimes later).
pub struct StepsRuntime {
    steps: Arc<dyn ContainerRuntime>,
    name: &'static str,
}

impl StepsRuntime {
    /// Runs steps with `steps`; `name` is reported in heartbeats.
    pub fn new(steps: Arc<dyn ContainerRuntime>, name: &'static str) -> Self {
        Self { steps, name }
    }
}

#[async_trait]
impl Runtime for StepsRuntime {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn version(&self) -> String {
        String::new()
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &[capability::BATCH_STEPS, capability::SECRETS]
    }

    async fn run(&self, job: &Job, ctx: &JobContext) -> Result<(), JobError> {
        run_job(job, ctx, self.steps.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_file_paths_stay_inside() {
        assert!(workspace_file_path("scripts/x.sh").is_some());
        assert!(workspace_file_path("x.sh").is_some());
        for bad in ["", "../x", "/etc/passwd", "a/../../x", "./x"] {
            assert!(workspace_file_path(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn secrets_become_global_env() {
        let env = global_env(&["A=1".into(), "B=x=y".into(), "broken".into()]);
        assert_eq!(env.len(), 2);
        assert_eq!(env["B"], "x=y");
    }

    #[test]
    fn errors_map_to_outcomes() {
        let failed = steps_error(StepsError::StepFailed {
            step: 0,
            exit_code: 3,
            stdout: String::new(),
            stderr: String::new(),
        });
        assert!(
            matches!(&failed, JobError::Failed(m) if m == "step 1 failed with exit code 3"),
            "{failed:?}"
        );
        assert!(matches!(
            steps_error(StepsError::Canceled),
            JobError::Canceled
        ));
        assert!(matches!(
            steps_error(StepsError::Image(RuntimeError::Image {
                image: "x".into(),
                message: "not found".into()
            })),
            JobError::Failed(_)
        ));
        assert!(matches!(
            steps_error(StepsError::Runtime {
                step: 1,
                error: RuntimeError::Other("daemon gone".into())
            }),
            JobError::Errored(_)
        ));
    }
}
