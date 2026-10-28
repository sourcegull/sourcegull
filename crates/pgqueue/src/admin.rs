//! Queue administration for the site admin "Background jobs" page.
//!
//! Queues are found by shape: every table in the current schema with the standard columns of
//! [`queue_columns_sql`](crate::queue_columns_sql) is a queue, so a new queue shows up without
//! being registered anywhere. For each, [`summarize`] counts rows by state and lists the
//! newest failures; [`retry`] and [`cancel`] act on one job.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::{Error, JobState, validate_table_name};

/// Columns that make a table a queue.
const QUEUE_COLUMNS: &[&str] = &[
    "state",
    "num_resets",
    "num_failures",
    "queued_at",
    "started_at",
    "finished_at",
    "process_after",
    "worker_hostname",
    "last_heartbeat_at",
    "failure_message",
    "execution_logs",
    "cancel",
];

/// A job as the admin page lists it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct QueueJob {
    pub id: i64,
    pub state: String,
    pub failure_message: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub num_failures: i32,
    pub num_resets: i32,
    pub worker_hostname: String,
}

/// A queue's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueSummary {
    /// The queue table.
    pub table: String,
    pub queued: i64,
    pub processing: i64,
    pub completed: i64,
    pub errored: i64,
    pub failed: i64,
    pub canceled: i64,
    /// The `queued_at` of the oldest queued job.
    pub oldest_queued_at: Option<DateTime<Utc>>,
    /// The newest failed or errored jobs, newest first.
    pub recent_failures: Vec<QueueJob>,
    /// The longest-waiting queued and processing jobs, oldest first.
    pub active: Vec<QueueJob>,
}

/// Every queue table in the current schema, by name.
pub async fn queue_tables(pool: &PgPool) -> Result<Vec<String>, Error> {
    let cols: Vec<String> = QUEUE_COLUMNS.iter().map(|c| (*c).to_owned()).collect();
    Ok(sqlx::query_scalar(
        "SELECT c.table_name::text FROM information_schema.columns c
         JOIN information_schema.tables t
           ON t.table_schema = c.table_schema AND t.table_name = c.table_name
         WHERE c.table_schema = current_schema() AND t.table_type = 'BASE TABLE'
           AND c.column_name = ANY($1)
         GROUP BY c.table_name
         HAVING count(DISTINCT c.column_name) = cardinality($1)
         ORDER BY c.table_name",
    )
    .bind(&cols)
    .fetch_all(pool)
    .await?)
}

const JOB_COLUMNS: &str = "id, state, failure_message, queued_at, started_at, finished_at, \
    num_failures, num_resets, worker_hostname";

/// Counts, recent failures and the oldest active jobs (up to `limit` each) of queue `table`.
pub async fn summarize(pool: &PgPool, table: &str, limit: i64) -> Result<QueueSummary, Error> {
    validate_table_name(table)?;
    let counts: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT state, count(*) FROM {table} GROUP BY state"
    ))
    .fetch_all(pool)
    .await?;
    let mut s = QueueSummary {
        table: table.to_owned(),
        ..Default::default()
    };
    for (state, n) in counts {
        match state.parse::<JobState>() {
            Ok(JobState::Queued) => s.queued = n,
            Ok(JobState::Processing) => s.processing = n,
            Ok(JobState::Completed) => s.completed = n,
            Ok(JobState::Errored) => s.errored = n,
            Ok(JobState::Failed) => s.failed = n,
            Ok(JobState::Canceled) => s.canceled = n,
            Err(_) => {}
        }
    }
    s.oldest_queued_at = sqlx::query_scalar(&format!(
        "SELECT min(queued_at) FROM {table} WHERE state = 'queued'"
    ))
    .fetch_one(pool)
    .await?;
    if limit > 0 && s.failed + s.errored > 0 {
        s.recent_failures = sqlx::query_as(&format!(
            "SELECT {JOB_COLUMNS} FROM {table} WHERE state IN ('failed', 'errored')
             ORDER BY finished_at DESC NULLS LAST, id DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(pool)
        .await?;
    }
    if limit > 0 && s.queued + s.processing > 0 {
        s.active = sqlx::query_as(&format!(
            "SELECT {JOB_COLUMNS} FROM {table} WHERE state IN ('queued', 'processing')
             ORDER BY queued_at, id LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(pool)
        .await?;
    }
    Ok(s)
}

/// Queues a failed, errored or canceled job again with fresh failure and reset counts.
/// Returns false if the job is in another state (or does not exist).
pub async fn retry(pool: &PgPool, table: &str, id: i64) -> Result<bool, Error> {
    validate_table_name(table)?;
    let r = sqlx::query(&format!(
        "UPDATE {table} SET state = 'queued', num_failures = 0, num_resets = 0,
             failure_message = NULL, process_after = NULL, started_at = NULL,
             finished_at = NULL, last_heartbeat_at = NULL, worker_hostname = '',
             cancel = false, queued_at = now()
         WHERE id = $1 AND state IN ('failed', 'errored', 'canceled')"
    ))
    .bind(id)
    .execute(pool)
    .await?;
    if r.rows_affected() > 0 {
        crate::notify_enqueued(pool, table).await?;
    }
    Ok(r.rows_affected() > 0)
}

/// Cancels a queued job at once, or asks the worker of a processing job to stop it (as
/// [`Queue::cancel`](crate::Queue::cancel)). Returns false if the job already finished.
pub async fn cancel(pool: &PgPool, table: &str, id: i64) -> Result<bool, Error> {
    validate_table_name(table)?;
    let r = sqlx::query(&format!(
        "UPDATE {table} SET cancel = true,
             state = CASE WHEN state = 'queued' THEN 'canceled' ELSE state END,
             finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
         WHERE id = $1 AND state IN ('queued', 'processing')"
    ))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}
