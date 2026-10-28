//! `changeset_jobs`: bulk operations over changesets of a bulk change.
//!
//! A bulk operation is one job per selected changeset, sharing a `bulk_group` id; its state
//! and progress are aggregated from the jobs. Workers dequeue with
//! `Queue::<ChangesetJob>::new(pool, ChangesetJobStore::queue_config())`.

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_pgqueue::{QueueConfig, Record, notify_enqueued};
use sourcegull_types::UserId;
use sqlx::types::Json;
use sqlx::{PgPool, Row};

use super::str_enum;

/// The queue table of bulk operation jobs.
pub const CHANGESET_JOBS_TABLE: &str = "changeset_jobs";

str_enum!(
    /// What a bulk operation does to each changeset.
    ChangesetJobType {
        /// Post a comment (`payload.message`).
        Comment => "comment",
        Detach => "detach",
        /// Re-queue a failed changeset for the reconciler.
        Reenqueue => "reenqueue",
        /// Merge (`payload.squash`).
        Merge => "merge",
        Close => "close",
        /// Publish (`payload.draft`).
        Publish => "publish",
    }
);

/// The payload of a bulk operation job.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangesetJob {
    pub bulk_group: String,
    pub user_id: UserId,
    pub batch_change_id: i64,
    pub changeset_id: i64,
    pub job_type: ChangesetJobType,
    pub payload: serde_json::Value,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ChangesetJob {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        let job_type: String = row.try_get("job_type")?;
        let Json(payload) = row.try_get("payload")?;
        Ok(Self {
            bulk_group: row.try_get("bulk_group")?,
            user_id: row.try_get("user_id")?,
            batch_change_id: row.try_get("batch_change_id")?,
            changeset_id: row.try_get("changeset_id")?,
            job_type: ChangesetJobType::parse(&job_type).ok_or_else(|| {
                sqlx::Error::ColumnDecode {
                    index: "job_type".into(),
                    source: format!("unknown changeset job type {job_type:?}").into(),
                }
            })?,
            payload,
        })
    }
}

/// A bulk operation job with its queue state.
pub type ChangesetJobRow = Record<ChangesetJob>;

/// A bulk operation, aggregated from its jobs.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkOperation {
    /// The `bulk_group` id.
    pub id: String,
    pub job_type: ChangesetJobType,
    pub user_id: UserId,
    pub batch_change_id: i64,
    /// `PROCESSING` while a job is unfinished, then `FAILED` if one failed, else `COMPLETED`.
    pub state: &'static str,
    /// Finished jobs / all jobs.
    pub progress: f64,
    pub changeset_count: i64,
    pub created_at: DateTime<Utc>,
    /// When the last job finished, once all have.
    pub finished_at: Option<DateTime<Utc>>,
}

const BULK_SELECT: &str = "SELECT bulk_group, min(job_type) AS job_type, min(user_id) AS user_id,
         min(batch_change_id) AS batch_change_id,
         count(*) AS total,
         count(*) FILTER (WHERE state IN ('completed', 'failed', 'canceled')) AS finished,
         count(*) FILTER (WHERE state IN ('failed', 'canceled')) AS failed,
         min(created_at) AS created_at, max(finished_at) AS finished_at
     FROM changeset_jobs";

fn bulk_from_row(row: &sqlx::postgres::PgRow) -> Result<BulkOperation> {
    let job_type: String = row.try_get("job_type")?;
    let total: i64 = row.try_get("total")?;
    let finished: i64 = row.try_get("finished")?;
    let failed: i64 = row.try_get("failed")?;
    let done = finished == total;
    Ok(BulkOperation {
        id: row.try_get("bulk_group")?,
        job_type: ChangesetJobType::parse(&job_type).ok_or(sourcegull_db::DbError::Invalid(
            "unknown changeset job type",
        ))?,
        user_id: row.try_get("user_id")?,
        batch_change_id: row.try_get("batch_change_id")?,
        state: if !done {
            "PROCESSING"
        } else if failed > 0 {
            "FAILED"
        } else {
            "COMPLETED"
        },
        progress: if total == 0 {
            1.0
        } else {
            finished as f64 / total as f64
        },
        changeset_count: total,
        created_at: row.try_get("created_at")?,
        finished_at: if done {
            row.try_get("finished_at")?
        } else {
            None
        },
    })
}

/// Store for bulk operation jobs.
#[derive(Debug, Clone)]
pub struct ChangesetJobStore {
    pool: PgPool,
}

impl ChangesetJobStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The bulk operations queue.
    pub fn queue_config() -> QueueConfig {
        QueueConfig::new(CHANGESET_JOBS_TABLE)
    }

    /// Queues a bulk operation over `changeset_ids` and wakes workers. Returns its id (the
    /// `bulk_group`).
    pub async fn create_bulk(
        &self,
        user: UserId,
        batch_change_id: i64,
        job_type: ChangesetJobType,
        payload: &serde_json::Value,
        changeset_ids: &[i64],
    ) -> Result<String> {
        let payload = if payload.is_object() {
            payload.clone()
        } else {
            serde_json::json!({})
        };
        let mut tx = self.pool.begin().await?;
        let group: String = sqlx::query_scalar("SELECT replace(gen_random_uuid()::text, '-', '')")
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO changeset_jobs (bulk_group, user_id, batch_change_id, changeset_id,
                 job_type, payload)
             SELECT $1, $2, $3, c, $4, $5 FROM unnest($6::bigint[]) c",
        )
        .bind(&group)
        .bind(user)
        .bind(batch_change_id)
        .bind(job_type.as_str())
        .bind(Json(payload))
        .bind(changeset_ids)
        .execute(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, CHANGESET_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(group)
    }

    /// The jobs of a bulk operation, in id order.
    pub async fn jobs(&self, bulk_group: &str) -> Result<Vec<ChangesetJobRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM changeset_jobs WHERE bulk_group = $1 ORDER BY id")
                .bind(bulk_group)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Looks up a bulk operation.
    pub async fn bulk_operation(&self, bulk_group: &str) -> Result<Option<BulkOperation>> {
        let row = sqlx::query(&format!(
            "{BULK_SELECT} WHERE bulk_group = $1 GROUP BY bulk_group"
        ))
        .bind(bulk_group)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(bulk_from_row).transpose()
    }

    /// A bulk change's bulk operations, newest first; `before` is the creation time of the
    /// last one of the previous page.
    pub async fn bulk_operations(
        &self,
        batch_change_id: i64,
        before: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<BulkOperation>> {
        let rows = sqlx::query(&format!(
            "SELECT * FROM ({BULK_SELECT} WHERE batch_change_id = $1 GROUP BY bulk_group) b
             WHERE $2::timestamptz IS NULL OR b.created_at < $2
             ORDER BY b.created_at DESC, b.bulk_group DESC
             LIMIT CASE WHEN $3::bigint > 0 THEN $3::bigint END"
        ))
        .bind(batch_change_id)
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(bulk_from_row).collect()
    }

    /// The number of a bulk change's bulk operations.
    pub async fn count_bulk_operations(&self, batch_change_id: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(DISTINCT bulk_group) FROM changeset_jobs WHERE batch_change_id = $1",
        )
        .bind(batch_change_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The failed jobs of a bulk operation: (changeset id, failure message).
    pub async fn errors(&self, bulk_group: &str) -> Result<Vec<(i64, String)>> {
        Ok(sqlx::query_as(
            "SELECT changeset_id, COALESCE(failure_message, '') FROM changeset_jobs
             WHERE bulk_group = $1 AND state IN ('failed', 'canceled') ORDER BY id",
        )
        .bind(bulk_group)
        .fetch_all(&self.pool)
        .await?)
    }
}
