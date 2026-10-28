//! Background work for the permissions sync: the scheduler (a
//! leader-elected singleton) and the `permission_sync_jobs` queue handler (every worker).

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{Db, PermsSyncJob, PermsSyncJobStore, PermsSyncSubject};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use tokio_util::sync::CancellationToken;

use crate::sync::{PermsSyncer, SyncError};

/// Lease name of the permissions sync scheduler singleton.
pub const PERMS_SYNC_SCHEDULER: &str = "perms-sync-scheduler";
/// Queue workers per process.
pub const WORKER_CONCURRENCY: usize = 4;

/// Every `permissions.syncScheduleInterval` seconds, queues scheduled syncs of the
/// `permissions.syncOldestUsers` users and `permissions.syncOldestRepos` repositories synced
/// longest ago, and prunes the job history to `permissions.syncJobsHistorySize` per subject,
/// until `cancel` fires. Run it as a singleton.
pub async fn run_scheduler(
    db: Db,
    syncer: PermsSyncer,
    config: SiteConfigHandle,
    cancel: CancellationToken,
) {
    loop {
        let interval = {
            let c = config.load();
            Duration::from_secs(u64::from(c.value.permissions_sync_schedule_interval.max(1)))
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        let _ = sourcegull_db::record_run(PERMS_SYNC_SCHEDULER, async {
            schedule_once(&db, &syncer, &config).await;
            Ok::<_, std::convert::Infallible>(())
        })
        .await;
    }
}

/// One scheduler pass (see [`run_scheduler`]).
pub async fn schedule_once(db: &Db, syncer: &PermsSyncer, config: &SiteConfigHandle) {
    let (users, repos, users_backoff, repos_backoff, history) = {
        let c = config.load();
        let v = &c.value;
        (
            i64::from(v.permissions_sync_oldest_users),
            i64::from(v.permissions_sync_oldest_repos),
            chrono::Duration::seconds(i64::from(v.permissions_sync_users_backoff_seconds)),
            chrono::Duration::seconds(i64::from(v.permissions_sync_repos_backoff_seconds)),
            i64::from(v.permissions_sync_jobs_history_size),
        )
    };
    let jobs = db.perms_sync_jobs();
    let now = Utc::now();
    if users > 0 {
        match jobs.enqueue_due_users(users, now - users_backoff).await {
            Ok(ids) if !ids.is_empty() => {
                tracing::debug!(count = ids.len(), "queued scheduled user permissions syncs");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "queueing user permissions syncs failed"),
        }
    }
    if repos > 0 {
        let services = match syncer.repo_centric_services().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "listing permissions providers failed");
                Vec::new()
            }
        };
        match jobs
            .enqueue_due_repos(&services, repos, now - repos_backoff)
            .await
        {
            Ok(ids) if !ids.is_empty() => {
                tracing::debug!(count = ids.len(), "queued scheduled repo permissions syncs");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "queueing repo permissions syncs failed"),
        }
    }
    match jobs.prune_history(history).await {
        Ok(n) if n > 0 => tracing::debug!(deleted = n, "pruned permissions sync history"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "pruning permissions sync history failed"),
    }
}

/// Processes `permission_sync_jobs` rows with a [`PermsSyncer`].
pub struct PermsSyncHandler {
    syncer: PermsSyncer,
    db: Db,
}

impl PermsSyncHandler {
    /// A handler running syncs with `syncer`.
    pub fn new(db: Db, syncer: PermsSyncer) -> Self {
        Self { syncer, db }
    }
}

#[async_trait]
impl Handler<PermsSyncJob> for PermsSyncHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<PermsSyncJob>) -> Result<(), JobError> {
        let Some(subject) = record.payload.subject() else {
            return Err(JobError::fatal("job has neither a user nor a repository"));
        };
        // One run per subject at a time (OAuth refresh tokens are single-use): a job queued
        // while another runs waits for it.
        let (user, repo) = match subject {
            PermsSyncSubject::User(u) => (Some(u), None),
            PermsSyncSubject::Repo(r) => (None, Some(r)),
        };
        let earlier: bool = sourcegull_db::sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM permission_sync_jobs
             WHERE state = 'processing' AND id < $3
               AND (user_id = $1 OR repository_id = $2))",
        )
        .bind(user)
        .bind(repo)
        .bind(record.id)
        .fetch_one(self.db.pool())
        .await
        .map_err(JobError::retry)?;
        if earlier {
            return Err(JobError::retry("another sync of this subject is running"));
        }
        let outcome = match subject {
            PermsSyncSubject::User(u) => self.syncer.sync_user(u).await,
            PermsSyncSubject::Repo(r) => self.syncer.sync_repo(r).await,
        };
        let jobs = self.db.perms_sync_jobs();
        match outcome {
            Ok(result) => {
                jobs.record_result(record.id, &result)
                    .await
                    .map_err(JobError::retry)?;
                Ok(())
            }
            Err(SyncError::Partial { result, message }) => {
                jobs.record_result(record.id, &result)
                    .await
                    .map_err(JobError::retry)?;
                // The scheduler retries later; don't hammer a failing code host.
                Err(JobError::fatal(message))
            }
            Err(SyncError::Db(e)) => Err(JobError::retry(e)),
            Err(e @ SyncError::Config(_)) => Err(JobError::fatal(e)),
        }
    }
}

/// The permissions sync queue worker and its stalled-job resetter, ready to run.
pub fn perms_sync_workers(
    db: &Db,
    syncer: PermsSyncer,
    worker_id: &str,
) -> Result<(Worker<PermsSyncJob>, Resetter<PermsSyncJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<PermsSyncJob> =
        Queue::new(db.pool().clone(), PermsSyncJobStore::queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = WORKER_CONCURRENCY;
    let worker = Worker::new(
        queue.clone(),
        PermsSyncHandler::new(db.clone(), syncer),
        opts,
    );
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
