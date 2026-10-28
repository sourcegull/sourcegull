//! Queue operations on one queue table.

use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::types::Json;
use sqlx::{FromRow, PgExecutor, Row};

use crate::Error;
use crate::schema::{channel_name, validate_table_name};

/// The payload of a queue row: the domain's own columns, decoded from the row.
///
/// Implemented for every `FromRow` type, so `#[derive(sqlx::FromRow)]` on a
/// struct naming the payload columns is enough.
pub trait Job: for<'r> FromRow<'r, PgRow> + Send + Unpin + 'static {}

impl<T> Job for T where T: for<'r> FromRow<'r, PgRow> + Send + Unpin + 'static {}

/// Lifecycle state of a queue row (the `state` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting to be dequeued (possibly delayed by `process_after`).
    Queued,
    /// Held by a worker that heartbeats it.
    Processing,
    /// Finished successfully.
    Completed,
    /// Reserved for domains that park errored rows; the worker retries via `Queued`.
    Errored,
    /// Gave up: out of retries or resets, or a fatal error.
    Failed,
    /// Canceled by a user before or while running.
    Canceled,
}

impl JobState {
    /// The value stored in the `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Processing => "processing",
            Self::Completed => "completed",
            Self::Errored => "errored",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

impl FromStr for JobState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Ok(match s {
            "queued" => Self::Queued,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "errored" => Self::Errored,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            other => return Err(Error::UnknownState(other.to_owned())),
        })
    }
}

/// One append-only entry of a job's `execution_logs`, shown in the UI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionLogEntry {
    /// Step identifier, e.g. `"step.git.fetch"`.
    pub key: String,
    /// Command line, if the step ran a command.
    pub command: Vec<String>,
    /// When the step started.
    pub start_time: Option<DateTime<Utc>>,
    /// Process exit code, if any.
    pub exit_code: Option<i32>,
    /// Captured output.
    pub out: String,
    /// Step duration in milliseconds.
    pub duration_ms: Option<i64>,
}

/// A queue row: the standard columns plus the domain payload `T`.
#[derive(Debug, Clone)]
pub struct Record<T> {
    pub id: i64,
    pub state: JobState,
    pub num_resets: i32,
    pub num_failures: i32,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub process_after: Option<DateTime<Utc>>,
    pub worker_hostname: String,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
    pub execution_logs: Vec<ExecutionLogEntry>,
    pub cancel: bool,
    /// The domain payload columns.
    pub payload: T,
}

impl<'r, T: Job> FromRow<'r, PgRow> for Record<T> {
    fn from_row(row: &'r PgRow) -> sqlx::Result<Self> {
        let state: String = row.try_get("state")?;
        let state = state
            .parse()
            .map_err(|e: Error| sqlx::Error::ColumnDecode {
                index: "state".into(),
                source: Box::new(e),
            })?;
        let Json(execution_logs) = row.try_get("execution_logs")?;
        Ok(Self {
            id: row.try_get("id")?,
            state,
            num_resets: row.try_get("num_resets")?,
            num_failures: row.try_get("num_failures")?,
            queued_at: row.try_get("queued_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            process_after: row.try_get("process_after")?,
            worker_hostname: row.try_get("worker_hostname")?,
            last_heartbeat_at: row.try_get("last_heartbeat_at")?,
            failure_message: row.try_get("failure_message")?,
            execution_logs,
            cancel: row.try_get("cancel")?,
            payload: T::from_row(row)?,
        })
    }
}

/// Settings for one queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// Queue table name; see [`validate_table_name`].
    pub table: String,
    /// Stall resets allowed before a job moves to `failed`.
    pub max_resets: i32,
    /// Failed attempts allowed in total; the attempt that reaches it moves the job to `failed`.
    pub max_failures: i32,
    /// A processing job without a heartbeat for this long is stalled.
    pub stalled_after: Duration,
    /// How often workers heartbeat in-flight jobs.
    pub heartbeat_interval: Duration,
    /// Delay before the first retry; doubles with each failure.
    pub backoff_base: Duration,
    /// Upper bound on the retry delay.
    pub backoff_max: Duration,
    /// Dequeue by the table's `priority` column first (higher first), then oldest first.
    /// The table must have an integer `priority` column; the domain migration should index
    /// `(priority DESC, queued_at, id) WHERE state = 'queued'`.
    pub by_priority: bool,
    /// Extra SQL predicate rows must match to be dequeued, e.g. to take only jobs this
    /// process is responsible for. It may reference `$3`, which is bound to
    /// [`dequeue_filter_arg`](Self::dequeue_filter_arg) as text. Set both with
    /// [`with_dequeue_filter`](Self::with_dequeue_filter).
    pub dequeue_filter: Option<String>,
    /// The `$3` argument of [`dequeue_filter`](Self::dequeue_filter).
    pub dequeue_filter_arg: Option<String>,
}

impl QueueConfig {
    /// Defaults: 5 s heartbeat, 30 s stall threshold.
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            max_resets: 3,
            max_failures: 3,
            stalled_after: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(5),
            backoff_base: Duration::from_secs(10),
            backoff_max: Duration::from_secs(3600),
            by_priority: false,
            dequeue_filter: None,
            dequeue_filter_arg: None,
        }
    }

    /// Enables [`by_priority`](Self::by_priority) ordering.
    pub fn with_priority(mut self) -> Self {
        self.by_priority = true;
        self
    }

    /// Restricts [`Queue::dequeue`] to rows matching `predicate` (trusted SQL from code, never
    /// user input), where `$3` is `arg` as text. Example: `repo_id IN (SELECT id FROM repo
    /// WHERE shard = $3::text)`. The predicate must reference `$3`.
    pub fn with_dequeue_filter(
        mut self,
        predicate: impl Into<String>,
        arg: impl Into<String>,
    ) -> Self {
        self.dequeue_filter = Some(predicate.into());
        self.dequeue_filter_arg = Some(arg.into());
        self
    }
}

/// Result of [`Queue::heartbeat`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heartbeat {
    /// Ids still processing under this worker. Missing ids were taken away
    /// (reset as stalled, or finished elsewhere) and should be abandoned.
    pub known: Vec<i64>,
    /// Subset of `known` whose cancellation was requested.
    pub canceled: Vec<i64>,
}

/// Result of [`Queue::reset_stalled`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StalledReset {
    /// Returned to `queued`.
    pub reset: Vec<i64>,
    /// Moved to `failed` after exceeding `max_resets`.
    pub failed: Vec<i64>,
    /// Moved to `canceled` because cancellation was requested.
    pub canceled: Vec<i64>,
}

/// Sends the wake-up notification for `table` (`NOTIFY queue_<table>`).
///
/// Call it with the transaction that inserted the rows: Postgres delivers it on commit.
pub async fn notify_enqueued<'e, E: PgExecutor<'e>>(executor: E, table: &str) -> Result<(), Error> {
    validate_table_name(table)?;
    sqlx::query("SELECT pg_notify($1, '')")
        .bind(channel_name(table))
        .execute(executor)
        .await?;
    Ok(())
}

struct Sql {
    dequeue: String,
    get: String,
    heartbeat: String,
    complete: String,
    errored: String,
    failed: String,
    canceled: String,
    requeue: String,
    cancel: String,
    add_log: String,
    update_log: String,
    reset_stalled: String,
    reset_orphans: String,
    depth: String,
}

impl Sql {
    fn new(t: &str, by_priority: bool, filter: Option<&str>) -> Self {
        let mine = "id = $1 AND state = 'processing' AND worker_hostname = $2";
        let order = if by_priority {
            "priority DESC, queued_at, id"
        } else {
            "queued_at, id"
        };
        let filter = filter.map_or_else(String::new, |f| format!("AND ({f})"));
        Self {
            dequeue: format!(
                "WITH c AS MATERIALIZED (
                    SELECT id FROM {t}
                    WHERE state = 'queued' AND NOT cancel
                      AND (process_after IS NULL OR process_after <= now())
                      {filter}
                    ORDER BY {order}
                    FOR UPDATE SKIP LOCKED
                    LIMIT $2)
                 UPDATE {t} SET state = 'processing', started_at = now(), finished_at = NULL,
                     last_heartbeat_at = now(), worker_hostname = $1
                 FROM c WHERE {t}.id = c.id
                 RETURNING {t}.*"
            ),
            get: format!("SELECT * FROM {t} WHERE id = $1"),
            heartbeat: format!(
                "UPDATE {t} SET last_heartbeat_at = now()
                 WHERE id = ANY($1) AND state = 'processing' AND worker_hostname = $2
                 RETURNING id, cancel"
            ),
            complete: format!(
                "UPDATE {t} SET state = 'completed', finished_at = now() WHERE {mine}"
            ),
            errored: format!(
                "UPDATE {t} SET
                     num_failures = num_failures + 1,
                     failure_message = $3,
                     state = CASE WHEN num_failures + 1 >= $4 THEN 'failed' ELSE 'queued' END,
                     finished_at = CASE WHEN num_failures + 1 >= $4 THEN now() END,
                     process_after = CASE WHEN num_failures + 1 >= $4 THEN process_after
                         ELSE now() + make_interval(secs => least($5::float8 * power(2::float8, num_failures), $6::float8))
                     END,
                     last_heartbeat_at = NULL
                 WHERE {mine}
                 RETURNING state"
            ),
            failed: format!(
                "UPDATE {t} SET state = 'failed', finished_at = now(),
                     num_failures = num_failures + 1, failure_message = $3
                 WHERE {mine}"
            ),
            canceled: format!(
                "UPDATE {t} SET state = 'canceled', finished_at = now() WHERE {mine}"
            ),
            requeue: format!(
                "UPDATE {t} SET state = 'queued', started_at = NULL, last_heartbeat_at = NULL
                 WHERE {mine}"
            ),
            cancel: format!(
                "UPDATE {t} SET cancel = true,
                     state = CASE WHEN state = 'queued' THEN 'canceled' ELSE state END,
                     finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
                 WHERE id = $1 AND state IN ('queued', 'processing')"
            ),
            add_log: format!(
                "UPDATE {t} SET execution_logs = execution_logs || jsonb_build_array($2::jsonb)
                 WHERE id = $1
                 RETURNING jsonb_array_length(execution_logs) - 1"
            ),
            update_log: format!(
                "UPDATE {t}
                 SET execution_logs = jsonb_set(execution_logs, ARRAY[$2::text], $3::jsonb)
                 WHERE id = $1 AND $2 >= 0 AND $2 < jsonb_array_length(execution_logs)"
            ),
            reset_stalled: format!(
                "WITH s AS MATERIALIZED (
                    SELECT id FROM {t}
                    WHERE state = 'processing'
                      AND last_heartbeat_at < now() - make_interval(secs => $1::float8)
                    FOR UPDATE SKIP LOCKED)
                 UPDATE {t} SET
                     num_resets = {t}.num_resets + 1,
                     state = CASE WHEN {t}.cancel THEN 'canceled'
                                  WHEN {t}.num_resets + 1 > $2 THEN 'failed'
                                  ELSE 'queued' END,
                     finished_at = CASE WHEN {t}.cancel OR {t}.num_resets + 1 > $2 THEN now() END,
                     failure_message = CASE WHEN NOT {t}.cancel AND {t}.num_resets + 1 > $2
                         THEN 'job stalled too many times (no heartbeat)'
                         ELSE {t}.failure_message END,
                     started_at = NULL,
                     last_heartbeat_at = NULL
                 FROM s WHERE {t}.id = s.id
                 RETURNING {t}.id, {t}.state"
            ),
            reset_orphans: format!(
                "UPDATE {t} SET
                     num_resets = num_resets + 1,
                     state = CASE WHEN cancel THEN 'canceled'
                                  WHEN num_resets + 1 > $4 THEN 'failed'
                                  ELSE 'queued' END,
                     finished_at = CASE WHEN cancel OR num_resets + 1 > $4 THEN now() END,
                     failure_message = CASE WHEN NOT cancel AND num_resets + 1 > $4
                         THEN 'job was lost by its worker too many times'
                         ELSE failure_message END,
                     started_at = NULL,
                     last_heartbeat_at = NULL
                 WHERE state = 'processing' AND worker_hostname = $1 AND NOT (id = ANY($2))
                   AND started_at < now() - make_interval(secs => $3::float8)
                 RETURNING id"
            ),
            depth: format!("SELECT count(*) FROM {t} WHERE state = 'queued'"),
        }
    }
}

pub(crate) struct Core {
    pub(crate) pool: PgPool,
    pub(crate) config: QueueConfig,
    sql: Sql,
}

impl Core {
    pub(crate) async fn add_execution_log(
        &self,
        id: i64,
        entry: &ExecutionLogEntry,
    ) -> Result<Option<i32>, Error> {
        let idx = sqlx::query_scalar(&self.sql.add_log)
            .bind(id)
            .bind(Json(entry))
            .fetch_optional(&self.pool)
            .await?;
        Ok(idx)
    }
}

/// A handle to one queue table whose payload columns decode as `T`.
///
/// Domains insert rows themselves (then call [`Queue::notify_enqueued`] or
/// [`notify_enqueued`]); this type moves rows through their lifecycle. Every
/// worker-side transition is guarded by `state = 'processing'` and the
/// worker id, so a worker cannot touch a job that was reset and handed to
/// another worker. Worker ids must be unique per process.
pub struct Queue<T> {
    pub(crate) core: Arc<Core>,
    _payload: PhantomData<fn() -> T>,
}

impl<T> Clone for Queue<T> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            _payload: PhantomData,
        }
    }
}

impl<T: Job> Queue<T> {
    /// Creates a handle; fails if the table name is not a plain identifier.
    pub fn new(pool: PgPool, config: QueueConfig) -> Result<Self, Error> {
        validate_table_name(&config.table)?;
        let sql = Sql::new(
            &config.table,
            config.by_priority,
            config.dequeue_filter.as_deref(),
        );
        Ok(Self {
            core: Arc::new(Core { pool, config, sql }),
            _payload: PhantomData,
        })
    }

    /// The queue's settings.
    pub fn config(&self) -> &QueueConfig {
        &self.core.config
    }

    /// The pool this queue runs its statements on.
    pub fn pool(&self) -> &PgPool {
        &self.core.pool
    }

    /// The `LISTEN` channel for this queue.
    pub fn channel(&self) -> String {
        channel_name(&self.core.config.table)
    }

    /// Wakes listening workers; see the free function [`notify_enqueued`].
    pub async fn notify_enqueued(&self) -> Result<(), Error> {
        notify_enqueued(&self.core.pool, &self.core.config.table).await
    }

    /// Claims up to `n` ready jobs for `worker_id`, oldest first (highest priority first with
    /// [`QueueConfig::by_priority`]; the returned batch is then in no particular order).
    ///
    /// Uses `FOR UPDATE SKIP LOCKED`, so concurrent dequeuers never get the same row.
    pub async fn dequeue(&self, worker_id: &str, n: usize) -> Result<Vec<Record<T>>, Error> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let mut query = sqlx::query_as(&self.core.sql.dequeue)
            .bind(worker_id)
            .bind(limit);
        if self.core.config.dequeue_filter.is_some() {
            query = query.bind(
                self.core
                    .config
                    .dequeue_filter_arg
                    .clone()
                    .unwrap_or_default(),
            );
        }
        let mut rows: Vec<Record<T>> = query.fetch_all(&self.core.pool).await?;
        if !self.core.config.by_priority {
            rows.sort_by_key(|r| (r.queued_at, r.id));
        }
        Ok(rows)
    }

    /// Loads one row by id.
    pub async fn get(&self, id: i64) -> Result<Option<Record<T>>, Error> {
        Ok(sqlx::query_as(&self.core.sql.get)
            .bind(id)
            .fetch_optional(&self.core.pool)
            .await?)
    }

    /// Refreshes `last_heartbeat_at` for the worker's in-flight jobs.
    pub async fn heartbeat(&self, ids: &[i64], worker_id: &str) -> Result<Heartbeat, Error> {
        if ids.is_empty() {
            return Ok(Heartbeat::default());
        }
        let rows: Vec<(i64, bool)> = sqlx::query_as(&self.core.sql.heartbeat)
            .bind(ids)
            .bind(worker_id)
            .fetch_all(&self.core.pool)
            .await?;
        let mut hb = Heartbeat::default();
        for (id, cancel) in rows {
            hb.known.push(id);
            if cancel {
                hb.canceled.push(id);
            }
        }
        hb.known.sort_unstable();
        hb.canceled.sort_unstable();
        Ok(hb)
    }

    async fn transition(&self, sql: &str, id: i64, worker_id: &str) -> Result<bool, Error> {
        let res = sqlx::query(sql)
            .bind(id)
            .bind(worker_id)
            .execute(&self.core.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Marks a job completed. Returns false if the worker no longer holds it.
    pub async fn mark_complete(&self, id: i64, worker_id: &str) -> Result<bool, Error> {
        self.transition(&self.core.sql.complete, id, worker_id)
            .await
    }

    /// Records a failed attempt. The job is requeued with exponential backoff
    /// (`process_after`), or moves to `failed` once `max_failures` is reached.
    /// Returns the new state, or `None` if the worker no longer holds the job.
    pub async fn mark_errored(
        &self,
        id: i64,
        worker_id: &str,
        message: &str,
    ) -> Result<Option<JobState>, Error> {
        let cfg = &self.core.config;
        let state: Option<String> = sqlx::query_scalar(&self.core.sql.errored)
            .bind(id)
            .bind(worker_id)
            .bind(message)
            .bind(cfg.max_failures)
            .bind(cfg.backoff_base.as_secs_f64())
            .bind(cfg.backoff_max.as_secs_f64())
            .fetch_optional(&self.core.pool)
            .await?;
        state.map(|s| s.parse()).transpose()
    }

    /// Moves a job straight to `failed` (no retry).
    pub async fn mark_failed(
        &self,
        id: i64,
        worker_id: &str,
        message: &str,
    ) -> Result<bool, Error> {
        let res = sqlx::query(&self.core.sql.failed)
            .bind(id)
            .bind(worker_id)
            .bind(message)
            .execute(&self.core.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Acknowledges a cancellation: the worker stopped the job.
    pub async fn mark_canceled(&self, id: i64, worker_id: &str) -> Result<bool, Error> {
        self.transition(&self.core.sql.canceled, id, worker_id)
            .await
    }

    /// Returns a held job to `queued` without counting a failure or reset
    /// (used when a worker shuts down mid-job).
    pub async fn requeue(&self, id: i64, worker_id: &str) -> Result<bool, Error> {
        self.transition(&self.core.sql.requeue, id, worker_id).await
    }

    /// Requests cancellation. A queued job becomes `canceled` at once; a
    /// processing job is flagged and its worker stops it at the next heartbeat.
    /// Returns false if the job was already finished.
    pub async fn cancel(&self, id: i64) -> Result<bool, Error> {
        let res = sqlx::query(&self.core.sql.cancel)
            .bind(id)
            .execute(&self.core.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Appends an entry to the job's execution logs and returns its index,
    /// or `None` if the job does not exist.
    pub async fn add_execution_log(
        &self,
        id: i64,
        entry: &ExecutionLogEntry,
    ) -> Result<Option<i32>, Error> {
        self.core.add_execution_log(id, entry).await
    }

    /// Replaces entry `index` of the job's execution logs (a step whose output grew).
    /// Returns false if there is no such job or entry.
    pub async fn update_execution_log(
        &self,
        id: i64,
        index: i32,
        entry: &ExecutionLogEntry,
    ) -> Result<bool, Error> {
        let res = sqlx::query(&self.core.sql.update_log)
            .bind(id)
            .bind(index)
            .bind(Json(entry))
            .execute(&self.core.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Returns stalled jobs (no heartbeat for `stalled_after`) to `queued`,
    /// or to `failed` once they exceed `max_resets`.
    pub async fn reset_stalled(&self) -> Result<StalledReset, Error> {
        let cfg = &self.core.config;
        let rows: Vec<(i64, String)> = sqlx::query_as(&self.core.sql.reset_stalled)
            .bind(cfg.stalled_after.as_secs_f64())
            .bind(cfg.max_resets)
            .fetch_all(&self.core.pool)
            .await?;
        let mut out = StalledReset::default();
        for (id, state) in rows {
            match state.parse()? {
                JobState::Queued => out.reset.push(id),
                JobState::Canceled => out.canceled.push(id),
                _ => out.failed.push(id),
            }
        }
        out.reset.sort_unstable();
        out.failed.sort_unstable();
        out.canceled.sort_unstable();
        Ok(out)
    }

    /// Resets jobs `worker_id` holds but no longer reports running (`running` lists the ones
    /// it does), as [`reset_stalled`](Self::reset_stalled) would. Jobs started less than
    /// `grace` ago are left alone: the worker may not have received them yet. Used for remote
    /// workers (executors) that report their jobs with each heartbeat.
    pub async fn reset_orphans(
        &self,
        worker_id: &str,
        running: &[i64],
        grace: Duration,
    ) -> Result<Vec<i64>, Error> {
        let mut ids: Vec<i64> = sqlx::query_scalar(&self.core.sql.reset_orphans)
            .bind(worker_id)
            .bind(running)
            .bind(grace.as_secs_f64())
            .bind(self.core.config.max_resets)
            .fetch_all(&self.core.pool)
            .await?;
        ids.sort_unstable();
        Ok(ids)
    }

    /// Number of rows in `queued` state (including delayed retries).
    pub async fn queue_depth(&self) -> Result<i64, Error> {
        Ok(sqlx::query_scalar(&self.core.sql.depth)
            .fetch_one(&self.core.pool)
            .await?)
    }
}
