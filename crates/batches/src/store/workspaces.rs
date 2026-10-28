//! `batch_spec_workspaces`, `batch_spec_workspace_execution_jobs` (the `batches` executor
//! queue) and `batch_spec_execution_cache_entries`.
//!
//! Resolution writes a spec's workspaces; executing the spec queues one job per workspace
//! that is neither skipped nor fully cached. Executors dequeue jobs fairly between users:
//! the next job belongs to the user whose last dequeue is the oldest (or who never had one).
//! A trigger keeps `batch_spec_workspace_execution_last_dequeues`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_pgqueue::{QueueConfig, Record, notify_enqueued};
use sourcegull_types::{RepoId, UserId};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Row};

use super::changeset_specs::{ChangesetSpec, NewChangesetSpec, insert_specs};
use super::like_pattern;

/// The queue table of workspace executions (the `batches` executor queue).
pub const BATCH_SPEC_EXECUTION_JOBS_TABLE: &str = "batch_spec_workspace_execution_jobs";

/// The job payload format written by [`BatchSpecWorkspaceStore::enqueue_executions`].
pub const EXECUTION_JOB_VERSION: i32 = 2;

/// A workspace to create.
#[derive(Debug, Clone, PartialEq)]
pub struct NewWorkspace {
    pub repo_id: RepoId,
    /// The branch ref (`refs/heads/…`).
    pub branch: String,
    pub commit: String,
    /// '' for the repository root.
    pub path: String,
    pub file_matches: Vec<String>,
    pub only_fetch_workspace: bool,
    pub ignored: bool,
    pub unsupported: bool,
    pub skipped: bool,
    pub skipped_reason: Option<String>,
    pub cached_result_found: bool,
    /// Per-step cache hits: `{"<step index>": <result>}`.
    pub step_cache_results: serde_json::Value,
}

impl NewWorkspace {
    /// The root workspace of `repo` at `commit` of `branch`, not skipped or cached.
    pub fn new(repo_id: RepoId, branch: impl Into<String>, commit: impl Into<String>) -> Self {
        Self {
            repo_id,
            branch: branch.into(),
            commit: commit.into(),
            path: String::new(),
            file_matches: Vec::new(),
            only_fetch_workspace: false,
            ignored: false,
            unsupported: false,
            skipped: false,
            skipped_reason: None,
            cached_result_found: false,
            step_cache_results: serde_json::json!({}),
        }
    }
}

/// A resolved workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchSpecWorkspace {
    pub id: i64,
    pub batch_spec_id: i64,
    pub repo_id: RepoId,
    pub branch: String,
    pub commit: String,
    pub path: String,
    pub file_matches: Vec<String>,
    pub only_fetch_workspace: bool,
    pub ignored: bool,
    pub unsupported: bool,
    pub skipped: bool,
    pub skipped_reason: Option<String>,
    pub cached_result_found: bool,
    pub step_cache_results: serde_json::Value,
    /// The changeset specs the execution (or the cache) produced, in id order.
    pub changeset_spec_ids: Vec<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for BatchSpecWorkspace {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        let Json(ids): Json<BTreeMap<String, serde_json::Value>> =
            row.try_get("changeset_spec_ids")?;
        let mut changeset_spec_ids: Vec<i64> = ids.keys().filter_map(|k| k.parse().ok()).collect();
        changeset_spec_ids.sort_unstable();
        let Json(step_cache_results) = row.try_get("step_cache_results")?;
        Ok(Self {
            id: row.try_get("id")?,
            batch_spec_id: row.try_get("batch_spec_id")?,
            repo_id: RepoId(row.try_get("repo_id")?),
            branch: row.try_get("branch")?,
            commit: row.try_get("commit")?,
            path: row.try_get("path")?,
            file_matches: row.try_get("file_matches")?,
            only_fetch_workspace: row.try_get("only_fetch_workspace")?,
            ignored: row.try_get("ignored")?,
            unsupported: row.try_get("unsupported")?,
            skipped: row.try_get("skipped")?,
            skipped_reason: row.try_get("skipped_reason")?,
            cached_result_found: row.try_get("cached_result_found")?,
            step_cache_results,
            changeset_spec_ids,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Filters of [`BatchSpecWorkspaceStore::list`]. Workspaces are listed in id order.
#[derive(Debug, Clone, Default)]
pub struct ListWorkspaces {
    pub batch_spec_id: i64,
    /// Only workspaces whose execution is in this state: a queue state (`queued`,
    /// `processing`, `completed`, `failed`, `canceled`), `skipped`, `cached` (result from
    /// the cache) or `pending` (no execution yet).
    pub state: Option<String>,
    /// Matches the repository name (case-insensitive substring).
    pub search: Option<String>,
    /// Only workspaces with an id above this cursor.
    pub after_id: Option<i64>,
    /// Page size; no limit when 0.
    pub limit: i64,
}

/// `WHERE` clause of [`ListWorkspaces`] over `batch_spec_workspaces w JOIN repo r`, with
/// `j` the workspace's latest execution job (lateral, may be NULL); binds `$1`..`$4`.
const LIST_FROM: &str = "batch_spec_workspaces w
    JOIN repo r ON r.id = w.repo_id AND r.deleted_at IS NULL
    LEFT JOIN LATERAL (
        SELECT state FROM batch_spec_workspace_execution_jobs j
        WHERE j.batch_spec_workspace_id = w.id ORDER BY j.id DESC LIMIT 1
    ) j ON true
    WHERE w.batch_spec_id = $1
      AND ($2::text IS NULL
           OR ($2 = 'skipped' AND w.skipped)
           OR ($2 = 'cached' AND NOT w.skipped AND w.cached_result_found)
           OR ($2 = 'pending' AND NOT w.skipped AND NOT w.cached_result_found AND j.state IS NULL)
           OR j.state = $2)
      AND ($3::text IS NULL OR r.name ILIKE $3)
      AND ($4::bigint IS NULL OR w.id > $4)";

/// The payload of a workspace execution job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionJob {
    pub batch_spec_workspace_id: i64,
    pub user_id: UserId,
    pub version: i32,
    /// Position among the user's queued jobs (1 = next), when listed with it.
    pub place_in_user_queue: Option<i64>,
    /// Position in the whole queue (1 = next), when listed with it.
    pub place_in_global_queue: Option<i64>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ExecutionJob {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        let optional = |name: &str| match row.try_get::<Option<i64>, _>(name) {
            Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
            other => other,
        };
        Ok(Self {
            batch_spec_workspace_id: row.try_get("batch_spec_workspace_id")?,
            user_id: row.try_get("user_id")?,
            version: row.try_get("version")?,
            place_in_user_queue: optional("place_in_user_queue")?,
            place_in_global_queue: optional("place_in_global_queue")?,
        })
    }
}

/// A workspace execution job with its queue state.
pub type ExecutionJobRow = Record<ExecutionJob>;

/// Counts over a spec's workspaces and executions, from which the spec's state follows.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct ExecutionStats {
    pub workspaces: i64,
    pub skipped_workspaces: i64,
    pub cached_workspaces: i64,
    /// Execution jobs (one per executed workspace).
    pub executions: i64,
    pub queued: i64,
    pub processing: i64,
    pub completed: i64,
    pub failed: i64,
    pub canceled: i64,
    /// Jobs with cancellation requested that are still running.
    pub canceling: i64,
    /// When the first job started and the last one finished.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Queue positions of queued jobs: within the user's own jobs, then interleaved between
/// users by their last dequeue (never-dequeued users first), matching the dequeue order.
const QUEUE_PLACES: &str = "WITH q AS (
        SELECT j.id, j.user_id, d.latest_dequeue,
            row_number() OVER (PARTITION BY j.user_id ORDER BY j.queued_at, j.id) AS place_in_user_queue
        FROM batch_spec_workspace_execution_jobs j
        LEFT JOIN batch_spec_workspace_execution_last_dequeues d ON d.user_id = j.user_id
        WHERE j.state = 'queued' AND NOT j.cancel
    ), places AS (
        SELECT id, place_in_user_queue, row_number() OVER (
            ORDER BY place_in_user_queue, latest_dequeue NULLS FIRST, user_id) AS place_in_global_queue
        FROM q
    )";

/// The fair-dequeue predicate: only jobs of the given version (`$3`) of the user whose last
/// dequeue is the oldest among users with ready jobs.
const FAIR_DEQUEUE: &str = "version = $3::int AND user_id = (
        SELECT q.user_id FROM batch_spec_workspace_execution_jobs q
        LEFT JOIN batch_spec_workspace_execution_last_dequeues d ON d.user_id = q.user_id
        WHERE q.state = 'queued' AND NOT q.cancel AND q.version = $3::int
          AND (q.process_after IS NULL OR q.process_after <= now())
        ORDER BY d.latest_dequeue NULLS FIRST, q.user_id
        LIMIT 1)";

/// Store for workspaces and their execution jobs.
#[derive(Debug, Clone)]
pub struct BatchSpecWorkspaceStore {
    pool: PgPool,
}

impl BatchSpecWorkspaceStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The `batches` executor queue for executors that run job payload `version`: fair
    /// between users, and with the longer stall threshold of executor queues.
    ///
    /// Concurrent dequeuers pick the same user; when that user's ready jobs are all locked
    /// by others, a dequeue returns nothing and the executor polls again.
    pub fn execution_queue_config(version: i32) -> QueueConfig {
        let mut config = QueueConfig::new(BATCH_SPEC_EXECUTION_JOBS_TABLE)
            .with_dequeue_filter(FAIR_DEQUEUE, version.to_string());
        config.stalled_after = std::time::Duration::from_secs(60);
        config
    }

    /// Inserts a spec's workspaces in one transaction. Returns their ids in input order.
    pub async fn create_many(
        &self,
        batch_spec_id: i64,
        workspaces: &[NewWorkspace],
    ) -> Result<Vec<i64>> {
        let mut tx = self.pool.begin().await?;
        let ids = insert_workspaces(&mut tx, batch_spec_id, workspaces).await?;
        tx.commit().await?;
        Ok(ids)
    }

    /// Stores the result of resolving a spec's workspaces in one transaction: replaces the
    /// spec's workspaces (their executions go with them) and its changeset specs that no
    /// changeset uses with `workspaces` and `imports` (the `importChangesets` specs). Returns
    /// the workspace ids in input order.
    pub async fn replace_resolution(
        &self,
        batch_spec_id: i64,
        workspaces: &[NewWorkspace],
        imports: &[NewChangesetSpec],
    ) -> Result<Vec<i64>> {
        let mut tx = self.pool.begin().await?;
        // Lock the spec so concurrent resolutions of it apply one after the other.
        sqlx::query("SELECT id FROM batch_specs WHERE id = $1 FOR UPDATE")
            .bind(batch_spec_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(sourcegull_db::DbError::NotFound("bulk change spec"))?;
        sqlx::query("DELETE FROM batch_spec_workspaces WHERE batch_spec_id = $1")
            .bind(batch_spec_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM changeset_specs s WHERE s.batch_spec_id = $1
               AND NOT EXISTS (SELECT 1 FROM changesets c
                               WHERE c.current_spec_id = s.id OR c.previous_spec_id = s.id)",
        )
        .bind(batch_spec_id)
        .execute(&mut *tx)
        .await?;
        let imports: Vec<NewChangesetSpec> = imports
            .iter()
            .map(|s| NewChangesetSpec {
                batch_spec_id: Some(batch_spec_id),
                ..s.clone()
            })
            .collect();
        insert_specs(&mut tx, &imports).await?;
        let ids = insert_workspaces(&mut tx, batch_spec_id, workspaces).await?;
        tx.commit().await?;
        Ok(ids)
    }

    /// Deletes a spec's workspaces with their executions (before resolving it again).
    pub async fn delete_for_spec(&self, batch_spec_id: i64) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM batch_spec_workspaces WHERE batch_spec_id = $1")
                .bind(batch_spec_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Looks up a workspace.
    pub async fn get(&self, id: i64) -> Result<Option<BatchSpecWorkspace>> {
        Ok(
            sqlx::query_as("SELECT * FROM batch_spec_workspaces WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Workspaces matching `opts`, in id order.
    pub async fn list(&self, opts: &ListWorkspaces) -> Result<Vec<BatchSpecWorkspace>> {
        Ok(sqlx::query_as(&format!(
            "SELECT w.* FROM {LIST_FROM} ORDER BY w.id
             LIMIT CASE WHEN $5::bigint > 0 THEN $5::bigint END"
        ))
        .bind(opts.batch_spec_id)
        .bind(&opts.state)
        .bind(opts.search.as_deref().and_then(like_pattern))
        .bind(opts.after_id)
        .bind(opts.limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of workspaces matching `opts` (ignoring `after_id` and `limit`).
    pub async fn count(&self, opts: &ListWorkspaces) -> Result<i64> {
        Ok(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {LIST_FROM}"))
                .bind(opts.batch_spec_id)
                .bind(&opts.state)
                .bind(opts.search.as_deref().and_then(like_pattern))
                .bind(None::<i64>)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Records the changeset specs a workspace's execution produced (replacing earlier ones).
    pub async fn set_changeset_specs(&self, workspace_id: i64, spec_ids: &[i64]) -> Result<bool> {
        let map: BTreeMap<String, serde_json::Value> = spec_ids
            .iter()
            .map(|id| (id.to_string(), serde_json::json!({})))
            .collect();
        Ok(sqlx::query(
            "UPDATE batch_spec_workspaces SET changeset_spec_ids = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(workspace_id)
        .bind(Json(map))
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Records cache hits found for a workspace before (or instead of) executing it.
    pub async fn set_cache_results(
        &self,
        workspace_id: i64,
        cached_result_found: bool,
        step_cache_results: &serde_json::Value,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE batch_spec_workspaces SET cached_result_found = $2, step_cache_results = $3,
                 updated_at = now() WHERE id = $1",
        )
        .bind(workspace_id)
        .bind(cached_result_found)
        .bind(Json(step_cache_object(step_cache_results)))
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Queues an execution job (payload [`EXECUTION_JOB_VERSION`]) for every workspace of the
    /// spec that is not skipped, has no cached result and has no job yet, run as `user`.
    /// Returns the number of jobs queued.
    pub async fn enqueue_executions(&self, batch_spec_id: i64, user: UserId) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "INSERT INTO batch_spec_workspace_execution_jobs (batch_spec_workspace_id, user_id, version)
             SELECT w.id, $2, $3 FROM batch_spec_workspaces w
             WHERE w.batch_spec_id = $1 AND NOT w.skipped AND NOT w.cached_result_found
               AND NOT EXISTS (SELECT 1 FROM batch_spec_workspace_execution_jobs j
                               WHERE j.batch_spec_workspace_id = w.id)
             ORDER BY w.id",
        )
        .bind(batch_spec_id)
        .bind(user)
        .bind(EXECUTION_JOB_VERSION)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        notify_enqueued(&mut *tx, BATCH_SPEC_EXECUTION_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Forgets the cache hits of a spec's workspaces (`executeBatchSpec(noCache: true)`), so
    /// every workspace that is not skipped gets an execution job.
    pub async fn clear_cache_results(&self, batch_spec_id: i64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE batch_spec_workspaces SET cached_result_found = false,
                 step_cache_results = '{}'::jsonb, updated_at = now()
             WHERE batch_spec_id = $1
               AND (cached_result_found OR step_cache_results <> '{}'::jsonb)",
        )
        .bind(batch_spec_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Queues an execution job for each of the given workspaces of the spec that is not
    /// skipped and has no job yet (`enqueueBatchSpecWorkspaceExecution`), cached or not.
    /// Returns the number of jobs queued.
    pub async fn enqueue_workspace_executions(
        &self,
        batch_spec_id: i64,
        workspace_ids: &[i64],
        user: UserId,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "INSERT INTO batch_spec_workspace_execution_jobs (batch_spec_workspace_id, user_id, version)
             SELECT w.id, $3, $4 FROM batch_spec_workspaces w
             WHERE w.batch_spec_id = $1 AND w.id = ANY($2) AND NOT w.skipped
               AND NOT EXISTS (SELECT 1 FROM batch_spec_workspace_execution_jobs j
                               WHERE j.batch_spec_workspace_id = w.id)
             ORDER BY w.id",
        )
        .bind(batch_spec_id)
        .bind(workspace_ids)
        .bind(user)
        .bind(EXECUTION_JOB_VERSION)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        notify_enqueued(&mut *tx, BATCH_SPEC_EXECUTION_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Replaces the changeset specs a workspace produced with `specs` (attached to the
    /// workspace's bulk change spec) in one transaction: earlier ones no changeset uses are deleted.
    /// Returns the stored specs in input order.
    pub async fn replace_changeset_specs(
        &self,
        workspace_id: i64,
        specs: &[NewChangesetSpec],
    ) -> Result<Vec<ChangesetSpec>> {
        let mut tx = self.pool.begin().await?;
        let batch_spec_id: i64 = sqlx::query_scalar(
            "SELECT batch_spec_id FROM batch_spec_workspaces WHERE id = $1 FOR UPDATE",
        )
        .bind(workspace_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(sourcegull_db::DbError::NotFound(
            "bulk change spec workspace",
        ))?;
        sqlx::query(
            "DELETE FROM changeset_specs s USING batch_spec_workspaces w
             WHERE w.id = $1 AND w.changeset_spec_ids ? s.id::text
               AND NOT EXISTS (SELECT 1 FROM changesets c
                               WHERE c.current_spec_id = s.id OR c.previous_spec_id = s.id)",
        )
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;
        let specs: Vec<NewChangesetSpec> = specs
            .iter()
            .map(|s| NewChangesetSpec {
                batch_spec_id: Some(batch_spec_id),
                ..s.clone()
            })
            .collect();
        let created = insert_specs(&mut tx, &specs).await?;
        let map: BTreeMap<String, serde_json::Value> = created
            .iter()
            .map(|s| (s.id.to_string(), serde_json::json!({})))
            .collect();
        sqlx::query(
            "UPDATE batch_spec_workspaces SET changeset_spec_ids = $2, updated_at = now()
             WHERE id = $1",
        )
        .bind(workspace_id)
        .bind(Json(map))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(created)
    }

    /// The failure messages of a spec's failed executions, oldest workspace first, at most
    /// `limit`.
    pub async fn failure_messages(&self, batch_spec_id: i64, limit: i64) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT j.failure_message FROM batch_spec_workspace_execution_jobs j
             JOIN batch_spec_workspaces w ON w.id = j.batch_spec_workspace_id
             WHERE w.batch_spec_id = $1 AND j.state = 'failed' AND j.failure_message IS NOT NULL
             ORDER BY w.id LIMIT $2",
        )
        .bind(batch_spec_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Looks up an execution job, with its queue places while queued.
    pub async fn execution_job(&self, id: i64) -> Result<Option<ExecutionJobRow>> {
        Ok(sqlx::query_as(&format!(
            "{QUEUE_PLACES}
             SELECT j.*, p.place_in_user_queue, p.place_in_global_queue
             FROM batch_spec_workspace_execution_jobs j LEFT JOIN places p ON p.id = j.id
             WHERE j.id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The latest execution job of each of the given workspaces, with queue places.
    pub async fn execution_jobs_for_workspaces(
        &self,
        workspace_ids: &[i64],
    ) -> Result<Vec<ExecutionJobRow>> {
        Ok(sqlx::query_as(&format!(
            "{QUEUE_PLACES}
             SELECT DISTINCT ON (j.batch_spec_workspace_id)
                 j.*, p.place_in_user_queue, p.place_in_global_queue
             FROM batch_spec_workspace_execution_jobs j LEFT JOIN places p ON p.id = j.id
             WHERE j.batch_spec_workspace_id = ANY($1)
             ORDER BY j.batch_spec_workspace_id, j.id DESC"
        ))
        .bind(workspace_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Counts over a spec's workspaces and executions.
    pub async fn stats(&self, batch_spec_id: i64) -> Result<ExecutionStats> {
        Ok(sqlx::query_as(
            "SELECT
                 (SELECT count(*) FROM batch_spec_workspaces w WHERE w.batch_spec_id = $1) AS workspaces,
                 (SELECT count(*) FROM batch_spec_workspaces w
                  WHERE w.batch_spec_id = $1 AND w.skipped) AS skipped_workspaces,
                 (SELECT count(*) FROM batch_spec_workspaces w
                  WHERE w.batch_spec_id = $1 AND NOT w.skipped AND w.cached_result_found) AS cached_workspaces,
                 count(j.id) AS executions,
                 count(j.id) FILTER (WHERE j.state IN ('queued', 'errored')) AS queued,
                 count(j.id) FILTER (WHERE j.state = 'processing') AS processing,
                 count(j.id) FILTER (WHERE j.state = 'completed') AS completed,
                 count(j.id) FILTER (WHERE j.state = 'failed') AS failed,
                 count(j.id) FILTER (WHERE j.state = 'canceled') AS canceled,
                 count(j.id) FILTER (WHERE j.state = 'processing' AND j.cancel) AS canceling,
                 min(j.started_at) AS started_at,
                 CASE WHEN count(j.id) FILTER (WHERE j.state IN ('queued', 'errored', 'processing')) = 0
                      THEN max(j.finished_at) END AS finished_at
             FROM batch_spec_workspaces w
             JOIN batch_spec_workspace_execution_jobs j ON j.batch_spec_workspace_id = w.id
             WHERE w.batch_spec_id = $1",
        )
        .bind(batch_spec_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Cancels a spec's executions: queued jobs become `canceled` and running ones are asked
    /// to stop (their executor sees it at the next heartbeat). Returns the jobs affected.
    pub async fn cancel_executions(&self, batch_spec_id: i64) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE batch_spec_workspace_execution_jobs j SET cancel = true,
                 state = CASE WHEN j.state = 'queued' THEN 'canceled' ELSE j.state END,
                 finished_at = CASE WHEN j.state = 'queued' THEN now() ELSE j.finished_at END,
                 updated_at = now()
             FROM batch_spec_workspaces w
             WHERE w.id = j.batch_spec_workspace_id AND w.batch_spec_id = $1
               AND j.state IN ('queued', 'processing')",
        )
        .bind(batch_spec_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Queues the given jobs again from scratch (when finished: failed, canceled, or also
    /// completed with `include_completed`), dropping the changeset specs their workspaces
    /// produced. `batch_spec_id` limits the jobs to one spec (`retryBatchSpecExecution`).
    /// Returns the number of jobs re-queued.
    pub async fn retry_executions(
        &self,
        batch_spec_id: i64,
        job_ids: Option<&[i64]>,
        include_completed: bool,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let workspaces: Vec<i64> = sqlx::query_scalar(
            "UPDATE batch_spec_workspace_execution_jobs j SET state = 'queued', num_resets = 0,
                 num_failures = 0, queued_at = now(), started_at = NULL, finished_at = NULL,
                 process_after = NULL, worker_hostname = '', last_heartbeat_at = NULL,
                 failure_message = NULL, execution_logs = '[]'::jsonb, cancel = false,
                 updated_at = now()
             FROM batch_spec_workspaces w
             WHERE w.id = j.batch_spec_workspace_id AND w.batch_spec_id = $1
               AND ($2::bigint[] IS NULL OR j.id = ANY($2))
               AND (j.state IN ('failed', 'canceled') OR ($3 AND j.state = 'completed'))
             RETURNING w.id",
        )
        .bind(batch_spec_id)
        .bind(job_ids)
        .bind(include_completed)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM changeset_specs s USING batch_spec_workspaces w
             WHERE w.id = ANY($1) AND w.changeset_spec_ids ? s.id::text
               AND NOT EXISTS (SELECT 1 FROM changesets c
                               WHERE c.current_spec_id = s.id OR c.previous_spec_id = s.id)",
        )
        .bind(&workspaces)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE batch_spec_workspaces SET changeset_spec_ids = '{}'::jsonb, updated_at = now()
             WHERE id = ANY($1)",
        )
        .bind(&workspaces)
        .execute(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, BATCH_SPEC_EXECUTION_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(workspaces.len() as u64)
    }
}

async fn insert_workspaces(
    conn: &mut PgConnection,
    batch_spec_id: i64,
    workspaces: &[NewWorkspace],
) -> Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(workspaces.len());
    for w in workspaces {
        ids.push(
            sqlx::query_scalar(
                "INSERT INTO batch_spec_workspaces (batch_spec_id, repo_id, branch, commit,
                     path, file_matches, only_fetch_workspace, ignored, unsupported, skipped,
                     skipped_reason, cached_result_found, step_cache_results)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
            )
            .bind(batch_spec_id)
            .bind(w.repo_id.get())
            .bind(&w.branch)
            .bind(&w.commit)
            .bind(&w.path)
            .bind(&w.file_matches)
            .bind(w.only_fetch_workspace)
            .bind(w.ignored)
            .bind(w.unsupported)
            .bind(w.skipped)
            .bind(&w.skipped_reason)
            .bind(w.cached_result_found)
            .bind(Json(step_cache_object(&w.step_cache_results)))
            .fetch_one(&mut *conn)
            .await?,
        );
    }
    Ok(ids)
}

/// `step_cache_results` must be a JSON object; anything else is stored as `{}`.
fn step_cache_object(v: &serde_json::Value) -> serde_json::Value {
    if v.is_object() {
        v.clone()
    } else {
        serde_json::json!({})
    }
}

/// A cached step result.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExecutionCacheEntry {
    pub id: i64,
    pub user_id: UserId,
    pub key: String,
    /// The serialized result (the format belongs to the step runner).
    pub value: String,
    /// The result format version.
    pub version: i32,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Store for `batch_spec_execution_cache_entries`: step results per user and cache key.
#[derive(Debug, Clone)]
pub struct ExecutionCacheStore {
    pool: PgPool,
}

impl ExecutionCacheStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Looks up an entry and marks it used.
    pub async fn get(&self, user: UserId, key: &str) -> Result<Option<ExecutionCacheEntry>> {
        Ok(sqlx::query_as(
            "UPDATE batch_spec_execution_cache_entries SET last_used_at = now()
             WHERE user_id = $1 AND key = $2 RETURNING *",
        )
        .bind(user)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks up several entries (missing keys are left out) and marks them used.
    pub async fn get_many(
        &self,
        user: UserId,
        keys: &[String],
    ) -> Result<Vec<ExecutionCacheEntry>> {
        Ok(sqlx::query_as(
            "UPDATE batch_spec_execution_cache_entries SET last_used_at = now()
             WHERE user_id = $1 AND key = ANY($2) RETURNING *",
        )
        .bind(user)
        .bind(keys)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Looks up several entries without marking them used (to show results).
    pub async fn peek_many(
        &self,
        user: UserId,
        keys: &[String],
    ) -> Result<Vec<ExecutionCacheEntry>> {
        Ok(sqlx::query_as(
            "SELECT * FROM batch_spec_execution_cache_entries WHERE user_id = $1 AND key = ANY($2)",
        )
        .bind(user)
        .bind(keys)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Stores an entry, replacing one with the same key.
    pub async fn set(&self, user: UserId, key: &str, value: &str, version: i32) -> Result<()> {
        sqlx::query(
            "INSERT INTO batch_spec_execution_cache_entries (user_id, key, value, version, last_used_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (user_id, key) DO UPDATE SET value = EXCLUDED.value,
                 version = EXCLUDED.version, last_used_at = now(), created_at = now()",
        )
        .bind(user)
        .bind(key)
        .bind(value)
        .bind(version)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes a user's entries with these keys (a spec run with `noCache`).
    pub async fn delete(&self, user: UserId, keys: &[String]) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM batch_spec_execution_cache_entries WHERE user_id = $1 AND key = ANY($2)",
        )
        .bind(user)
        .bind(keys)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Evicts least recently used entries until the values fit in `max_bytes`, and entries
    /// with a format version below `min_version`. Returns the number deleted.
    pub async fn cleanup(&self, max_bytes: i64, min_version: i32) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM batch_spec_execution_cache_entries WHERE version < $2 OR id IN (
                 SELECT id FROM (
                     SELECT id, sum(octet_length(value)) OVER (
                         ORDER BY COALESCE(last_used_at, created_at) DESC, id DESC) AS running
                     FROM batch_spec_execution_cache_entries
                 ) s WHERE s.running > $1)",
        )
        .bind(max_bytes)
        .bind(min_version)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
