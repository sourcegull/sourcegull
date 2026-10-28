//! `external_service_sync_jobs` store: the discovery sync queue and its history.
//!
//! The table is a [`sourcegull_pgqueue`] queue: a leader-elected scheduler calls
//! [`SyncJobStore::enqueue_due`], admins call [`SyncJobStore::enqueue`] ("sync now"), and
//! workers process rows with a `Queue<SyncJob>` built from [`SyncJobStore::queue_config`]. At
//! most one row per connection is queued at a time.

use crate::Result;
use chrono::{DateTime, Utc};
use sourcegull_pgqueue::{QueueConfig, notify_enqueued};
use sourcegull_types::{ExternalServiceId, ExternalServiceSyncJobId};
use sqlx::{PgConnection, PgPool};

/// The queue table.
pub const SYNC_JOBS_TABLE: &str = "external_service_sync_jobs";

/// Why a sync was queued (`external_service_sync_jobs.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncReason {
    /// The connection's sync interval elapsed.
    Scheduled,
    /// An admin asked for it.
    Manual,
    /// The connection was created or its configuration changed.
    ConfigChanged,
    /// A code host event (e.g. a GitLab system hook) announced a change.
    Webhook,
}

impl SyncReason {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
            Self::ConfigChanged => "config_changed",
            Self::Webhook => "webhook",
        }
    }
}

/// The payload of a sync job, for `Queue<SyncJob>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SyncJob {
    pub external_service_id: ExternalServiceId,
    pub reason: String,
}

/// Counters a sync run records on its row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Whether the run was a full listing.
    pub full: bool,
    /// Repos the code host returned (after exclusions).
    pub synced: i32,
    pub added: i32,
    pub modified: i32,
    pub deleted: i32,
    pub unmodified: i32,
    /// Errors the code host returned while listing.
    pub errors: i32,
}

/// One sync run as shown in the sync history.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SyncJobRecord {
    pub id: ExternalServiceSyncJobId,
    pub external_service_id: ExternalServiceId,
    pub state: String,
    pub reason: String,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
    pub full_sync: Option<bool>,
    pub repos_synced: i32,
    pub repos_added: i32,
    pub repos_modified: i32,
    pub repos_deleted: i32,
    pub repos_unmodified: i32,
    pub repo_sync_errors: i32,
    /// Problems that did not fail the run.
    pub warnings: Vec<String>,
    /// Whether cancellation was requested (a processing run then stops at its next heartbeat).
    pub cancel: bool,
}

/// The columns of [`SyncJobRecord`].
const RECORD_COLUMNS: &str = "id, external_service_id, state, reason, queued_at, started_at, \
     finished_at, failure_message, full_sync, repos_synced, repos_added, repos_modified, \
     repos_deleted, repos_unmodified, repo_sync_errors, warnings, cancel";

/// Reads and writes `external_service_sync_jobs`.
#[derive(Debug, Clone)]
pub struct SyncJobStore {
    pool: PgPool,
}

impl SyncJobStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The queue settings workers use for this table.
    pub fn queue_config() -> QueueConfig {
        let mut config = QueueConfig::new(SYNC_JOBS_TABLE);
        // A large code host can take minutes to list; a failed run is retried by the next
        // scheduled one anyway.
        config.max_failures = 1;
        config
    }

    /// Queues a sync of a live connection unless one is already queued. Returns the new job
    /// id, or `None` when a job was already queued (or the connection does not exist).
    pub async fn enqueue(
        &self,
        id: ExternalServiceId,
        reason: SyncReason,
    ) -> Result<Option<ExternalServiceSyncJobId>> {
        let mut tx = self.pool.begin().await?;
        let job = enqueue_sync_job(&mut tx, id, reason).await?;
        tx.commit().await?;
        Ok(job)
    }

    /// Queues a scheduled sync for every live connection whose `next_sync_at` has passed and
    /// that has no queued or running sync. Returns the connections queued.
    pub async fn enqueue_due(&self) -> Result<Vec<ExternalServiceId>> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<ExternalServiceId> = sqlx::query_scalar(
            "INSERT INTO external_service_sync_jobs (external_service_id, reason)
             SELECT es.id, 'scheduled' FROM external_services es
             WHERE es.deleted_at IS NULL AND es.next_sync_at <= now()
               AND NOT EXISTS (
                   SELECT 1 FROM external_service_sync_jobs j
                   WHERE j.external_service_id = es.id AND j.state IN ('queued', 'processing'))
             ON CONFLICT DO NOTHING
             RETURNING external_service_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        if !ids.is_empty() {
            notify_enqueued(&mut *tx, SYNC_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Records a run's counters on its row.
    pub async fn record_stats(&self, job: i64, stats: &SyncStats) -> Result<()> {
        sqlx::query(
            "UPDATE external_service_sync_jobs SET full_sync = $2, repos_synced = $3,
                 repos_added = $4, repos_modified = $5, repos_deleted = $6, repos_unmodified = $7,
                 repo_sync_errors = $8
             WHERE id = $1",
        )
        .bind(job)
        .bind(stats.full)
        .bind(stats.synced)
        .bind(stats.added)
        .bind(stats.modified)
        .bind(stats.deleted)
        .bind(stats.unmodified)
        .bind(stats.errors)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records the problems a run reported that did not fail it.
    pub async fn record_warnings(&self, job: i64, warnings: &[String]) -> Result<()> {
        sqlx::query("UPDATE external_service_sync_jobs SET warnings = $2 WHERE id = $1")
            .bind(job)
            .bind(warnings)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The runs of a connection, newest first, skipping `offset`.
    pub async fn list(
        &self,
        id: ExternalServiceId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<SyncJobRecord>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RECORD_COLUMNS} FROM external_service_sync_jobs
             WHERE external_service_id = $1 ORDER BY id DESC LIMIT $2 OFFSET $3"
        ))
        .bind(id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of runs a connection has in its history.
    pub async fn count(&self, id: ExternalServiceId) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM external_service_sync_jobs WHERE external_service_id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// One run by id.
    pub async fn get(&self, job: ExternalServiceSyncJobId) -> Result<Option<SyncJobRecord>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RECORD_COLUMNS} FROM external_service_sync_jobs WHERE id = $1"
        ))
        .bind(job)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Requests cancellation of a queued or processing run: a queued run is canceled at once,
    /// a processing one is stopped by its worker at the next heartbeat. Returns whether the
    /// run was queued or processing.
    pub async fn cancel(&self, job: ExternalServiceSyncJobId) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE external_service_sync_jobs SET cancel = true,
                 state = CASE WHEN state = 'queued' THEN 'canceled' ELSE state END,
                 finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
             WHERE id = $1 AND state IN ('queued', 'processing')",
        )
        .bind(job)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Deletes finished runs older than `before`, keeping each connection's newest
    /// `keep_per_service` runs. Returns the number deleted.
    pub async fn delete_finished_before(
        &self,
        before: DateTime<Utc>,
        keep_per_service: i64,
    ) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM external_service_sync_jobs j USING (
                 SELECT id, row_number() OVER (PARTITION BY external_service_id ORDER BY id DESC) AS n
                 FROM external_service_sync_jobs) ranked
             WHERE j.id = ranked.id AND ranked.n > $2 AND j.finished_at < $1
               AND j.state IN ('completed', 'failed', 'canceled')",
        )
        .bind(before)
        .bind(keep_per_service)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

/// Inserts a queued sync job for a live connection in `tx` (no-op when one is queued) and
/// notifies workers on commit.
pub(crate) async fn enqueue_sync_job(
    tx: &mut PgConnection,
    id: ExternalServiceId,
    reason: SyncReason,
) -> Result<Option<ExternalServiceSyncJobId>> {
    let job: Option<ExternalServiceSyncJobId> = sqlx::query_scalar(
        "INSERT INTO external_service_sync_jobs (external_service_id, reason)
         SELECT id, $2 FROM external_services WHERE id = $1 AND deleted_at IS NULL
         ON CONFLICT DO NOTHING
         RETURNING id",
    )
    .bind(id)
    .bind(reason.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    if job.is_some() {
        notify_enqueued(&mut *tx, SYNC_JOBS_TABLE).await?;
    }
    Ok(job)
}
