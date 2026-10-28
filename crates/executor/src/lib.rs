//! The executor: dequeues jobs from the web role's executor queues and runs them.
//!
//! [`run`] polls each queue for jobs while fewer than `max_jobs` run, sends a heartbeat every
//! few seconds with the running jobs, stops the jobs the server cancels or no longer knows,
//! and reports each job's outcome: `markComplete`, `markFailed` when a step fails (rerunning
//! would fail the same way), `markErrored` (retried) for infrastructure errors. On shutdown
//! running jobs are stopped and marked errored so another executor retries them.
//!
//! Runtimes implement [`Runtime`]: [`docker::DockerRuntime`] runs steps as containers,
//! [`kubernetes::KubernetesRuntime`] runs each job as a Kubernetes Job. Jobs of the `batches`
//! queue (bulk change spec workspaces) run with the batch step runner ([`batches`]) on
//! runtimes that advertise [`capability::BATCH_STEPS`] (Docker, and Kubernetes with a Job per
//! step, [`kubernetes::steps`]).

pub mod batches;
pub mod checkout;
pub mod client;
pub mod docker;
pub mod kubernetes;
pub mod logger;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sourcegull_executor_api::{HeartbeatRequest, Job, QueueJobIds, capability};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub use client::{Client, ClientError};
pub use logger::{JobLogger, Redactor};

/// How a job ended, if not successfully.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// A step failed: reported with `markFailed`.
    #[error("{0}")]
    Failed(String),
    /// Infrastructure trouble (network, Docker): reported with `markErrored` and retried.
    #[error("{0}")]
    Errored(String),
    /// Stopped because the server canceled the job or took it away; nothing is reported.
    #[error("canceled")]
    Canceled,
    /// The server no longer gives this executor the job.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// What a runtime needs to run one job.
pub struct JobContext {
    /// The executor's name.
    pub executor: String,
    pub client: Client,
    pub logger: JobLogger,
    /// Fires when the job must stop.
    pub cancel: CancellationToken,
    /// A private scratch directory, removed after the job.
    pub scratch: PathBuf,
    /// The git binary used for checkouts.
    pub git: PathBuf,
}

/// Runs jobs somewhere: Docker containers, Kubernetes Jobs.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// `docker` or `kubernetes`, reported in heartbeats.
    fn name(&self) -> &'static str;
    /// The runtime's version, reported in heartbeats.
    async fn version(&self) -> String;
    /// What the runtime can run, advertised when dequeueing (see [`capability`]).
    fn capabilities(&self) -> &'static [&'static str] {
        &[
            capability::DOCKER_STEPS,
            capability::SCIP_UPLOAD,
            capability::SECRETS,
        ]
    }
    /// Checks out the job's commit, runs its steps and uploads its results.
    async fn run(&self, job: &Job, ctx: &JobContext) -> Result<(), JobError>;
}

/// Executor settings.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// The web role's URL, e.g. `http://sourcegull-web:7080`.
    pub url: url::Url,
    /// The shared executor token (`executors.accessToken`).
    pub token: String,
    /// Unique name of this executor.
    pub name: String,
    /// Queues to take jobs from.
    pub queues: Vec<String>,
    /// Jobs run at the same time.
    pub max_jobs: usize,
    /// Wait between dequeue attempts when the queues are empty.
    pub poll_interval: Duration,
    /// Interval between heartbeats.
    pub heartbeat_interval: Duration,
    /// Parent directory of the jobs' scratch directories.
    pub scratch_dir: PathBuf,
    /// The git binary.
    pub git: PathBuf,
}

/// A running job's stop signal, and whether the server asked for the stop (a cancellation)
/// rather than taking the job away.
#[derive(Clone, Default)]
struct Stop {
    token: CancellationToken,
    by_server: Arc<AtomicBool>,
}

type Running = Arc<Mutex<HashMap<(String, i64), Stop>>>;

/// Runs the executor until `shutdown` fires.
pub async fn run(
    config: ExecutorConfig,
    runtime: Arc<dyn Runtime>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(!config.queues.is_empty(), "no queues to take jobs from");
    tokio::fs::create_dir_all(&config.scratch_dir).await?;
    let client = Client::new(config.url.clone(), &config.token, &config.name);
    let running: Running = Arc::default();
    let jobs_stop = CancellationToken::new();
    let heartbeats = tokio::spawn(heartbeat_loop(
        config.clone(),
        client.clone(),
        runtime.clone(),
        running.clone(),
        shutdown.clone(),
    ));
    tracing::info!(executor = %config.name, queues = ?config.queues, url = %config.url,
        runtime = runtime.name(), "executor started");

    let slots = Arc::new(Semaphore::new(config.max_jobs.max(1)));
    let mut tasks = JoinSet::new();
    let mut next_queue = 0usize;
    'poll: loop {
        let permit = tokio::select! {
            p = slots.clone().acquire_owned() => p?,
            _ = shutdown.cancelled() => break,
        };
        // Try every queue once, starting with the one after the last that had a job.
        let mut job = None;
        for i in 0..config.queues.len() {
            let queue = &config.queues[(next_queue + i) % config.queues.len()];
            match client.dequeue(queue, runtime.capabilities()).await {
                Ok(Some(j)) => {
                    next_queue = (next_queue + i + 1) % config.queues.len();
                    job = Some(j);
                    break;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, queue, "dequeue failed"),
            }
        }
        let Some(job) = job else {
            drop(permit);
            tokio::select! {
                _ = tokio::time::sleep(config.poll_interval) => continue 'poll,
                _ = shutdown.cancelled() => break,
            }
        };
        let stop = Stop {
            token: jobs_stop.child_token(),
            by_server: Arc::default(),
        };
        let cancel = stop.token.clone();
        running
            .lock()
            .expect("running jobs")
            .insert((job.queue.clone(), job.id), stop.clone());
        let ctx = JobContext {
            executor: config.name.clone(),
            client: client.clone(),
            logger: JobLogger::new(client.clone(), &job.queue, job.id, Redactor::for_job(&job)),
            cancel,
            scratch: config.scratch_dir.join(format!(
                "{}-{}-{}",
                job.queue,
                job.id,
                uuid::Uuid::new_v4().simple()
            )),
            git: config.git.clone(),
        };
        let runtime = runtime.clone();
        let running = running.clone();
        let shutdown = shutdown.clone();
        tasks.spawn(async move {
            run_job(&*runtime, &job, &ctx, &shutdown, &stop.by_server).await;
            running
                .lock()
                .expect("running jobs")
                .remove(&(job.queue.clone(), job.id));
            drop(permit);
        });
        // Reap finished tasks so the set does not grow.
        while tasks.try_join_next().is_some() {}
    }
    tracing::info!("executor stopping; stopping running jobs");
    jobs_stop.cancel();
    while tasks.join_next().await.is_some() {}
    heartbeats.abort();
    Ok(())
}

/// Runs one job and reports its outcome. `canceled` tells whether the server canceled it.
async fn run_job(
    runtime: &dyn Runtime,
    job: &Job,
    ctx: &JobContext,
    shutdown: &CancellationToken,
    canceled: &AtomicBool,
) {
    tracing::info!(queue = %job.queue, job = job.id, repo = %job.repository_name, "running job");
    if let Err(e) = tokio::fs::create_dir_all(&ctx.scratch).await {
        tracing::error!(error = %e, "creating the scratch directory failed");
    }
    let result =
        if job.batches.is_some() && !runtime.capabilities().contains(&capability::BATCH_STEPS) {
            Err(JobError::Failed(format!(
                "the {} runtime cannot run bulk change spec steps",
                runtime.name()
            )))
        } else {
            runtime.run(job, ctx).await
        };
    let _ = tokio::fs::remove_dir_all(&ctx.scratch).await;
    let client = &ctx.client;
    let report = match &result {
        Ok(()) => client.mark_complete(&job.queue, job.id).await,
        Err(JobError::Canceled) if shutdown.is_cancelled() => {
            client
                .mark_errored(&job.queue, job.id, "The executor shut down.", false)
                .await
        }
        // A canceled batches job is reported as failed, which the server records as
        // canceled (the contract of the `batches` queue).
        Err(JobError::Canceled) if job.batches.is_some() && canceled.load(Ordering::Acquire) => {
            client
                .mark_errored(&job.queue, job.id, "The execution was canceled.", true)
                .await
        }
        // Canceled by the server or taken away: nothing to report (resetting a job whose
        // cancellation was requested cancels it).
        Err(JobError::Canceled | JobError::Client(ClientError::NotHeld)) => Ok(()),
        Err(JobError::Failed(m)) => client.mark_errored(&job.queue, job.id, m, true).await,
        Err(e) => {
            client
                .mark_errored(&job.queue, job.id, &e.to_string(), false)
                .await
        }
    };
    match (&result, report) {
        (_, Err(e)) => {
            tracing::warn!(error = %e, queue = %job.queue, job = job.id, "reporting the job outcome failed");
        }
        (Ok(()), Ok(())) => tracing::info!(queue = %job.queue, job = job.id, "job completed"),
        (Err(e), Ok(())) => {
            tracing::info!(queue = %job.queue, job = job.id, error = %e, "job did not complete");
        }
    }
}

async fn heartbeat_loop(
    config: ExecutorConfig,
    client: Client,
    runtime: Arc<dyn Runtime>,
    running: Running,
    shutdown: CancellationToken,
) {
    let runtime_version = runtime.version().await;
    let git_version = git_version(&config.git).await;
    let mut tick = tokio::time::interval(config.heartbeat_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.cancelled() => return,
        }
        let snapshot: Vec<(String, i64)> = running
            .lock()
            .expect("running jobs")
            .keys()
            .cloned()
            .collect();
        let mut jobs: Vec<QueueJobIds> = config
            .queues
            .iter()
            .map(|q| QueueJobIds {
                queue_name: q.clone(),
                job_ids: Vec::new(),
            })
            .collect();
        for (queue, id) in &snapshot {
            if let Some(j) = jobs.iter_mut().find(|j| j.queue_name == *queue) {
                j.job_ids.push(*id);
            }
        }
        let req = HeartbeatRequest {
            executor_name: config.name.clone(),
            queue_names: config.queues.clone(),
            jobs,
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            runtime: runtime.name().into(),
            runtime_version: runtime_version.clone(),
            git_version: git_version.clone(),
        };
        let resp = match client.heartbeat(&req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "heartbeat failed");
                continue;
            }
        };
        let listed = |lists: &[QueueJobIds], queue: &str, id: i64| {
            lists
                .iter()
                .any(|l| l.queue_name == queue && l.job_ids.contains(&id))
        };
        let running = running.lock().expect("running jobs");
        for (queue, id) in &snapshot {
            let stop = if listed(&resp.cancel, queue, *id) {
                tracing::info!(queue, job = id, "job canceled");
                if let Some(s) = running.get(&(queue.clone(), *id)) {
                    s.by_server.store(true, Ordering::Release);
                }
                true
            } else if !listed(&resp.known, queue, *id) {
                tracing::warn!(
                    queue,
                    job = id,
                    "the server no longer knows the job; stopping it"
                );
                true
            } else {
                false
            };
            if stop && let Some(s) = running.get(&(queue.clone(), *id)) {
                s.token.cancel();
            }
        }
    }
}

async fn git_version(git: &std::path::Path) -> String {
    tokio::process::Command::new(git)
        .arg("version")
        .output()
        .await
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_start_matches("git version ")
                .to_owned()
        })
        .unwrap_or_default()
}
