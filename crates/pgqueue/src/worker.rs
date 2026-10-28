//! Worker runtime: dequeue loop, heartbeats, cancellation and the stalled-job resetter.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use tokio::sync::Notify;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::queue::{Core, ExecutionLogEntry, Job, JobState, Queue, Record};

/// How a handler reports failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// Transient: retried with backoff until `max_failures`.
    #[error("{0}")]
    Retry(String),
    /// Permanent: the job moves to `failed` without retrying.
    #[error("{0}")]
    Fatal(String),
}

impl JobError {
    /// A retryable error from any displayable value.
    pub fn retry(e: impl Display) -> Self {
        Self::Retry(e.to_string())
    }

    /// A permanent error from any displayable value.
    pub fn fatal(e: impl Display) -> Self {
        Self::Fatal(e.to_string())
    }
}

/// Processes one job.
#[async_trait]
pub trait Handler<T: Job>: Send + Sync + 'static {
    /// Runs the job. Should return promptly once `ctx` is cancelled; after
    /// [`WorkerOptions::cancel_grace`] the handler future is dropped.
    async fn handle(&self, ctx: JobContext, record: Record<T>) -> Result<(), JobError>;
}

/// Per-job context passed to a [`Handler`].
pub struct JobContext {
    id: i64,
    worker_id: Arc<str>,
    cancel: CancellationToken,
    core: Arc<Core>,
}

impl JobContext {
    /// The job's row id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The worker running the job.
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Cancelled when the job is canceled, taken away (reset) or the worker shuts down.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Whether the job should stop.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when the job should stop.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Appends an execution log entry to this job; returns its index.
    pub async fn log(&self, entry: &ExecutionLogEntry) -> Result<Option<i32>, Error> {
        self.core.add_execution_log(self.id, entry).await
    }
}

/// Worker settings.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Maximum jobs handled at once.
    pub concurrency: usize,
    /// Idle poll interval; the fallback when notifications are missed.
    pub poll_interval: Duration,
    /// Unique id of this worker process, stored in `worker_hostname`.
    pub worker_id: String,
    /// How long a cancelled handler may keep running before it is dropped.
    pub cancel_grace: Duration,
}

impl WorkerOptions {
    /// Concurrency 1, 5 s poll, 10 s cancel grace.
    pub fn new(worker_id: impl Into<String>) -> Self {
        Self {
            concurrency: 1,
            poll_interval: Duration::from_secs(5),
            worker_id: worker_id.into(),
            cancel_grace: Duration::from_secs(10),
        }
    }
}

/// A worker id unique to this process: `<HOSTNAME>-<pid>-<nanos>`.
pub fn default_worker_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "sourcegull".to_owned());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!("{host}-{}-{nanos:x}", std::process::id())
}

// Why an in-flight job's token was cancelled. Stored in an AtomicU8; the
// first reason wins.
const RUNNING: u8 = 0;
const CANCELED: u8 = 1;
const LOST: u8 = 2;
const SHUTDOWN: u8 = 3;

struct InFlight {
    token: CancellationToken,
    reason: Arc<AtomicU8>,
    started: Instant,
}

impl InFlight {
    fn stop(&self, reason: u8) {
        let _ = self
            .reason
            .compare_exchange(RUNNING, reason, Ordering::AcqRel, Ordering::Acquire);
        self.token.cancel();
    }
}

type InFlightMap = Arc<Mutex<HashMap<i64, InFlight>>>;

fn lock(map: &InFlightMap) -> std::sync::MutexGuard<'_, HashMap<i64, InFlight>> {
    map.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs a [`Handler`] over a [`Queue`].
///
/// Wakes on `NOTIFY queue_<table>` through a dedicated `LISTEN` connection
/// taken from the queue's pool (so that pool must connect to Postgres
/// directly, not through a transaction-pooling PgBouncer), and polls every
/// `poll_interval` as a fallback. After a listener reconnect it polls at once,
/// since notifications sent meanwhile are lost.
pub struct Worker<T: Job> {
    queue: Queue<T>,
    handler: Arc<dyn Handler<T>>,
    opts: WorkerOptions,
}

impl<T: Job> Worker<T> {
    /// Creates a worker. A concurrency of 0 is treated as 1.
    pub fn new(queue: Queue<T>, handler: impl Handler<T>, mut opts: WorkerOptions) -> Self {
        opts.concurrency = opts.concurrency.max(1);
        Self {
            queue,
            handler: Arc::new(handler),
            opts,
        }
    }

    /// Runs until `cancel` fires. On shutdown it stops dequeuing, cancels
    /// in-flight handlers, waits for them (bounded by `cancel_grace`) and
    /// requeues the jobs that did not finish.
    pub async fn run(self, cancel: CancellationToken) {
        let worker_id: Arc<str> = self.opts.worker_id.as_str().into();
        let in_flight: InFlightMap = Arc::default();
        let wake = Arc::new(Notify::new());
        let background = CancellationToken::new();

        let listener = tokio::spawn(listen_loop(
            self.queue.pool().clone(),
            self.queue.channel(),
            wake.clone(),
            background.clone(),
        ));
        let heartbeats = tokio::spawn(heartbeat_loop(
            self.queue.clone(),
            in_flight.clone(),
            worker_id.clone(),
            self.opts.cancel_grace,
            background.clone(),
        ));

        let mut tasks = JoinSet::new();
        let mut tick = tokio::time::interval(self.opts.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let free = self.opts.concurrency.saturating_sub(tasks.len());
            if free > 0 && !cancel.is_cancelled() {
                match self.queue.dequeue(&worker_id, free).await {
                    Ok(records) => {
                        for record in records {
                            self.spawn_job(&mut tasks, &in_flight, &worker_id, record);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(table = %self.queue.config().table, error = %e, "dequeue failed")
                    }
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = wake.notified() => {}
                _ = tick.tick() => {}
                Some(res) = tasks.join_next(), if !tasks.is_empty() => log_task_exit(res),
            }
        }

        for job in lock(&in_flight).values() {
            job.stop(SHUTDOWN);
        }
        while let Some(res) = tasks.join_next().await {
            log_task_exit(res);
        }
        background.cancel();
        let _ = listener.await;
        let _ = heartbeats.await;
    }

    fn spawn_job(
        &self,
        tasks: &mut JoinSet<()>,
        in_flight: &InFlightMap,
        worker_id: &Arc<str>,
        record: Record<T>,
    ) {
        let id = record.id;
        let token = CancellationToken::new();
        let reason = Arc::new(AtomicU8::new(RUNNING));
        lock(in_flight).insert(
            id,
            InFlight {
                token: token.clone(),
                reason: reason.clone(),
                started: Instant::now(),
            },
        );
        let ctx = JobContext {
            id,
            worker_id: worker_id.clone(),
            cancel: token.clone(),
            core: self.queue.core.clone(),
        };
        let handler = self.handler.clone();
        let queue = self.queue.clone();
        let in_flight = in_flight.clone();
        let worker_id = worker_id.clone();
        let grace = self.opts.cancel_grace;
        tasks.spawn(async move {
            let mut handle = tokio::spawn(async move { handler.handle(ctx, record).await });
            let finished = tokio::select! {
                r = &mut handle => Some(r),
                _ = async { token.cancelled().await; tokio::time::sleep(grace).await } => None,
            };
            let outcome = match finished {
                Some(r) => r,
                None => {
                    handle.abort();
                    handle.await
                }
            };
            lock(&in_flight).remove(&id);
            finish(
                &queue,
                id,
                &worker_id,
                reason.load(Ordering::Acquire),
                outcome,
            )
            .await;
        });
    }
}

/// Records a job's outcome in the queue table.
async fn finish<T: Job>(
    queue: &Queue<T>,
    id: i64,
    worker_id: &str,
    reason: u8,
    outcome: Result<Result<(), JobError>, JoinError>,
) {
    let table = &queue.config().table;
    let res = match (reason, outcome) {
        (LOST, _) => {
            tracing::info!(%table, id, "job taken away from this worker; abandoned");
            return;
        }
        (CANCELED, _) => queue.mark_canceled(id, worker_id).await.map(drop),
        (_, Ok(Ok(()))) => queue.mark_complete(id, worker_id).await.map(drop),
        (SHUTDOWN, _) => queue.requeue(id, worker_id).await.map(drop),
        (_, Ok(Err(JobError::Retry(msg)))) => match queue.mark_errored(id, worker_id, &msg).await {
            Ok(Some(JobState::Failed)) => {
                tracing::warn!(%table, id, error = %msg, "job failed, out of retries");
                Ok(())
            }
            other => other.map(drop),
        },
        (_, Ok(Err(JobError::Fatal(msg)))) => {
            queue.mark_failed(id, worker_id, &msg).await.map(drop)
        }
        (_, Err(e)) => {
            let msg = if e.is_panic() {
                "handler panicked".to_owned()
            } else {
                format!("handler aborted: {e}")
            };
            queue.mark_errored(id, worker_id, &msg).await.map(drop)
        }
    };
    if let Err(e) = res {
        tracing::warn!(%table, id, error = %e, "recording job outcome failed");
    }
}

fn log_task_exit(res: Result<(), JoinError>) {
    if let Err(e) = res {
        tracing::error!(error = %e, "job task ended abnormally");
    }
}

/// Turns notifications on the queue's channel into wake-ups. The `LISTEN` connection is shared
/// by every subscriber in the process ([`sourcegull_pubsub`]), so workers do not pin pool
/// connections.
async fn listen_loop(pool: PgPool, channel: String, wake: Arc<Notify>, stop: CancellationToken) {
    let mut sub = tokio::select! {
        _ = stop.cancelled() => return,
        s = sourcegull_pubsub::subscribe(&pool, &[&channel]) => s,
    };
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            // A notification, or a resync after missed ones: dequeue either way.
            _ = sub.recv() => wake.notify_one(),
        }
    }
}

/// Heartbeats in-flight jobs and stops those canceled or taken away.
///
/// While heartbeats fail, a job is stopped as lost once it has gone unconfirmed long enough
/// that, with the cancel grace and one more interval, it would reach `stalled_after`: the
/// resetter of another process may then hand it to another worker, and the two must not run
/// at once.
async fn heartbeat_loop<T: Job>(
    queue: Queue<T>,
    in_flight: InFlightMap,
    worker_id: Arc<str>,
    cancel_grace: Duration,
    stop: CancellationToken,
) {
    let interval = queue.config().heartbeat_interval;
    let unconfirmed_limit = queue
        .config()
        .stalled_after
        .saturating_sub(cancel_grace)
        .saturating_sub(interval)
        .max(interval);
    let mut last_ok = Instant::now();
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        let ids: Vec<i64> = lock(&in_flight).keys().copied().collect();
        if ids.is_empty() {
            continue;
        }
        let sent = Instant::now();
        let hb = tokio::time::timeout(unconfirmed_limit, queue.heartbeat(&ids, &worker_id));
        match hb.await {
            Ok(Ok(hb)) => {
                last_ok = sent;
                let map = lock(&in_flight);
                for id in &ids {
                    let Some(job) = map.get(id) else { continue };
                    if hb.known.binary_search(id).is_err() {
                        job.stop(LOST);
                    } else if hb.canceled.binary_search(id).is_ok() {
                        job.stop(CANCELED);
                    }
                }
            }
            failed => {
                let table = &queue.config().table;
                match failed {
                    Ok(Err(e)) => tracing::warn!(%table, error = %e, "heartbeat failed"),
                    _ => tracing::warn!(%table, "heartbeat timed out"),
                }
                let map = lock(&in_flight);
                for (id, job) in map.iter() {
                    if job.started.max(last_ok).elapsed() >= unconfirmed_limit {
                        tracing::warn!(%table, id, "job unconfirmed too long; abandoned");
                        job.stop(LOST);
                    }
                }
            }
        }
    }
}

/// Periodically returns stalled jobs to the queue ([`Queue::reset_stalled`]).
///
/// Safe to run in every process: rows are claimed with `SKIP LOCKED`.
pub struct Resetter<T: Job> {
    queue: Queue<T>,
    interval: Duration,
}

impl<T: Job> Resetter<T> {
    /// Creates a resetter that runs every `interval`.
    pub fn new(queue: Queue<T>, interval: Duration) -> Self {
        Self { queue, interval }
    }

    /// Runs until `cancel` fires.
    pub async fn run(self, cancel: CancellationToken) {
        let table = &self.queue.config().table;
        loop {
            match self.queue.reset_stalled().await {
                Ok(r) => {
                    if !r.reset.is_empty() || !r.failed.is_empty() || !r.canceled.is_empty() {
                        tracing::info!(%table, reset = ?r.reset, failed = ?r.failed,
                            canceled = ?r.canceled, "reset stalled jobs");
                    }
                }
                Err(e) => tracing::warn!(%table, error = %e, "reset stalled jobs failed"),
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(self.interval) => {}
            }
        }
    }
}
