//! Background work for discovery: the sync scheduler (a leader-elected
//! singleton) and the sync queue handler (runs in every worker).

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_db::{Db, SyncJob, SyncJobStore};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use tokio_util::sync::CancellationToken;

use crate::sync::{SyncError, Syncer};

/// Lease name of the sync scheduler singleton.
pub const SYNC_SCHEDULER: &str = "repo-sync-scheduler";
/// How often the scheduler queues due syncs.
pub const SCHEDULER_INTERVAL: Duration = Duration::from_secs(10);
/// How often the scheduler prunes old rows.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);
/// How long webhook logs, ref events and sync history are kept.
pub const RETENTION: chrono::Duration = chrono::Duration::days(7);
/// How long finished repo update jobs are kept.
pub const UPDATE_JOB_RETENTION: chrono::Duration = chrono::Duration::days(1);
/// Sync runs kept per connection regardless of age.
pub const SYNC_HISTORY_KEEP: i64 = 20;

/// Queues due syncs every [`SCHEDULER_INTERVAL`] and prunes old rows every
/// [`RETENTION_INTERVAL`] until `cancel` fires. Run it as a singleton.
pub async fn run_scheduler(db: Db, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(SCHEDULER_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_prune: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(SYNC_SCHEDULER, db.sync_jobs().enqueue_due()).await {
            Ok(ids) if !ids.is_empty() => {
                tracing::debug!(count = ids.len(), "queued scheduled code host syncs");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "queueing scheduled syncs failed"),
        }
        if last_prune.is_none_or(|t| t.elapsed() >= RETENTION_INTERVAL) {
            last_prune = Some(tokio::time::Instant::now());
            prune(&db).await;
        }
    }
}

/// Deletes webhook logs, ref events and job history past their retention.
pub async fn prune(db: &Db) {
    let now = Utc::now();
    let results = [
        (
            "webhook_logs",
            db.webhook_logs().delete_before(now - RETENTION).await,
        ),
        (
            "repo_ref_events",
            db.ref_events().delete_before(now - RETENTION).await,
        ),
        (
            "repo_update_jobs",
            db.repo_update_jobs()
                .delete_finished_before(now - UPDATE_JOB_RETENTION)
                .await,
        ),
        (
            "external_service_sync_jobs",
            db.sync_jobs()
                .delete_finished_before(now - RETENTION, SYNC_HISTORY_KEEP)
                .await,
        ),
    ];
    for (table, result) in results {
        match result {
            Ok(n) if n > 0 => tracing::info!(table, deleted = n, "pruned old rows"),
            Ok(_) => {}
            Err(e) => tracing::warn!(table, error = %e, "pruning failed"),
        }
    }
}

/// Processes `external_service_sync_jobs` rows with a [`Syncer`].
pub struct SyncHandler {
    syncer: Syncer,
    db: Db,
}

impl SyncHandler {
    /// A handler running syncs with `syncer`.
    pub fn new(db: Db, syncer: Syncer) -> Self {
        Self { syncer, db }
    }
}

#[async_trait]
impl Handler<SyncJob> for SyncHandler {
    async fn handle(&self, ctx: JobContext, record: Record<SyncJob>) -> Result<(), JobError> {
        let id = record.payload.external_service_id;
        // At most one sync per connection runs: the lowest processing job id wins and the
        // others complete as no-ops.
        let earlier: bool = sourcegull_db::sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM external_service_sync_jobs
             WHERE external_service_id = $1 AND state = 'processing' AND id < $2)",
        )
        .bind(id)
        .bind(record.id)
        .fetch_one(self.db.pool())
        .await
        .map_err(JobError::retry)?;
        if earlier {
            tracing::info!(
                external_service = id.get(),
                "a sync of this connection is already running"
            );
            return Ok(());
        }
        match self
            .syncer
            .sync(id, Some(record.id), ctx.cancellation())
            .await
        {
            Ok(_) => Ok(()),
            Err(SyncError::Cancelled) => Err(JobError::retry("cancelled")),
            Err(SyncError::Db(e)) => Err(JobError::retry(e)),
            Err(e) => Err(JobError::fatal(e)),
        }
    }
}

/// The sync queue worker and its stalled-job resetter, ready to run.
pub fn sync_workers(
    db: &Db,
    syncer: Syncer,
    worker_id: &str,
) -> Result<(Worker<SyncJob>, Resetter<SyncJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<SyncJob> = Queue::new(db.pool().clone(), SyncJobStore::queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = 4;
    let worker = Worker::new(queue.clone(), SyncHandler::new(db.clone(), syncer), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
