//! `codeintel_index_jobs` store: auto-indexing jobs, the `codeintel` executor queue.
//!
//! A job is a [`sourcegull_pgqueue`] row (`queued` → `processing` → `completed` | `errored` |
//! `failed` | `canceled`). Executors hold it through the executor queue API; the job's upload
//! points back at it with `associated_index_id`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_db::{Db, Result};
use sourcegull_pgqueue::{ExecutionLogEntry, QueueConfig, notify_enqueued};
use sourcegull_types::{RepoAccess, RepoId, UserId};
use sqlx::PgPool;
use sqlx::types::Json;

/// The queue table of index jobs.
pub const INDEX_JOBS_TABLE: &str = "codeintel_index_jobs";

/// A step run in its own container before the indexer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexDockerStep {
    /// Directory relative to the repository root ('' for the root).
    pub root: String,
    /// Container image.
    pub image: String,
    /// Shell commands, run in order with `set -e`.
    pub commands: Vec<String>,
}

/// The payload columns of an index job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexJob {
    pub repo_id: RepoId,
    pub commit: String,
    pub root: String,
    pub indexer: String,
    pub indexer_args: Vec<String>,
    pub outfile: String,
    pub docker_steps: Vec<IndexDockerStep>,
    pub local_steps: Vec<String>,
    pub requested_env_vars: Vec<String>,
    pub should_reindex: bool,
    pub enqueuer: Option<UserId>,
    pub associated_upload_id: Option<i64>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for IndexJob {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let Json(docker_steps) = row.try_get("docker_steps")?;
        Ok(Self {
            repo_id: RepoId(row.try_get("repository_id")?),
            commit: row.try_get("commit")?,
            root: row.try_get("root")?,
            indexer: row.try_get("indexer")?,
            indexer_args: row.try_get("indexer_args")?,
            outfile: row.try_get("outfile")?,
            docker_steps,
            local_steps: row.try_get("local_steps")?,
            requested_env_vars: row.try_get("requested_env_vars")?,
            should_reindex: row.try_get("should_reindex")?,
            enqueuer: row
                .try_get::<Option<i64>, _>("enqueuer_user_id")?
                .map(UserId),
            associated_upload_id: row.try_get("associated_upload_id")?,
        })
    }
}

/// An index job row with its queue state, for listings.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexJobRow {
    pub id: i64,
    pub state: String,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
    pub worker_hostname: String,
    pub execution_logs: Vec<ExecutionLogEntry>,
    pub job: IndexJob,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for IndexJobRow {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let Json(execution_logs) = row.try_get("execution_logs")?;
        Ok(Self {
            id: row.try_get("id")?,
            state: row.try_get("state")?,
            queued_at: row.try_get("queued_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            failure_message: row.try_get("failure_message")?,
            worker_hostname: row.try_get("worker_hostname")?,
            execution_logs,
            job: IndexJob::from_row(row)?,
        })
    }
}

/// Filters of [`IndexJobStore::list`] and [`IndexJobStore::matching_ids`].
#[derive(Debug, Clone, Default)]
pub struct ListIndexJobs {
    pub repo: Option<RepoId>,
    /// Only these states (`queued`, `processing`, `errored`, …; any when empty).
    pub states: Vec<String>,
    /// Matches the commit, root, indexer or repository name (case-insensitive substring).
    pub query: Option<String>,
    /// Only jobs whose indexer image has one of these names (`scip-go` for
    /// `sourcegraph/scip-go@sha256:…`; any when empty).
    pub indexers: Vec<String>,
    /// Only jobs that have not produced an upload (with an upload, the upload stands for
    /// both).
    pub without_upload: bool,
    pub limit: i64,
    pub offset: i64,
}

/// The `WHERE` clause of [`ListIndexJobs`] over `codeintel_index_jobs j`, binds `$1`..`$6`.
const LIST_FILTER: &str = "($1::int IS NULL OR j.repository_id = $1)
    AND ($2::int[] IS NULL OR j.repository_id = ANY($2))
    AND (cardinality($3::text[]) = 0 OR j.state = ANY($3))
    AND ($4::text IS NULL OR j.commit ILIKE $4 OR j.root ILIKE $4 OR j.indexer ILIKE $4
         OR EXISTS (SELECT 1 FROM repo r WHERE r.id = j.repository_id AND r.name ILIKE $4))
    AND (cardinality($5::text[]) = 0 OR split_part(split_part(
            regexp_replace(j.indexer, '^sourcegraph/', ''), '@', 1), ':', 1) = ANY($5))
    AND (NOT $6 OR j.associated_upload_id IS NULL)
    AND EXISTS (SELECT 1 FROM repo r WHERE r.id = j.repository_id AND r.deleted_at IS NULL)";

fn like_pattern(q: Option<&str>) -> Option<String> {
    q.map(str::trim).filter(|q| !q.is_empty()).map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    })
}

/// Store for `codeintel_index_jobs`.
#[derive(Debug, Clone)]
pub struct IndexJobStore {
    pool: PgPool,
}

impl IndexJobStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The executor queue: FIFO, three attempts, and a longer stall threshold
    /// than in-process queues because heartbeats cross the network.
    pub fn queue_config() -> QueueConfig {
        let mut config = QueueConfig::new(INDEX_JOBS_TABLE);
        config.stalled_after = std::time::Duration::from_secs(60);
        config
    }

    /// Inserts a queued job and wakes listeners. Returns its id.
    pub async fn enqueue(&self, job: &IndexJob) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query_scalar(
            "INSERT INTO codeintel_index_jobs (repository_id, commit, root, indexer,
                indexer_args, outfile, docker_steps, local_steps, requested_env_vars,
                should_reindex, enqueuer_user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
        )
        .bind(job.repo_id.get())
        .bind(&job.commit)
        .bind(&job.root)
        .bind(&job.indexer)
        .bind(&job.indexer_args)
        .bind(&job.outfile)
        .bind(Json(&job.docker_steps))
        .bind(&job.local_steps)
        .bind(&job.requested_env_vars)
        .bind(job.should_reindex)
        .bind(job.enqueuer.map(|u| u.get()))
        .fetch_one(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, INDEX_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Loads one job.
    pub async fn get(&self, id: i64) -> Result<Option<IndexJobRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM codeintel_index_jobs WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Inserts queued jobs in one transaction and wakes listeners. Returns their ids.
    pub async fn enqueue_many(&self, jobs: &[IndexJob]) -> Result<Vec<i64>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        let mut ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            ids.push(
                sqlx::query_scalar(
                    "INSERT INTO codeintel_index_jobs (repository_id, commit, root, indexer,
                        indexer_args, outfile, docker_steps, local_steps, requested_env_vars,
                        should_reindex, enqueuer_user_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
                )
                .bind(job.repo_id.get())
                .bind(&job.commit)
                .bind(&job.root)
                .bind(&job.indexer)
                .bind(&job.indexer_args)
                .bind(&job.outfile)
                .bind(Json(&job.docker_steps))
                .bind(&job.local_steps)
                .bind(&job.requested_env_vars)
                .bind(job.should_reindex)
                .bind(job.enqueuer.map(|u| u.get()))
                .fetch_one(&mut *tx)
                .await?,
            );
        }
        notify_enqueued(&mut *tx, INDEX_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(ids)
    }

    /// Loads jobs by id (missing ones are left out), in id order.
    pub async fn get_many(&self, ids: &[i64]) -> Result<Vec<IndexJobRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM codeintel_index_jobs WHERE id = ANY($1) ORDER BY id")
                .bind(ids)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Jobs matching `opts` that `access` may see, newest first, and their total count.
    pub async fn list(
        &self,
        opts: &ListIndexJobs,
        access: &RepoAccess,
    ) -> Result<(Vec<IndexJobRow>, i64)> {
        let readable = access.readable_ids();
        let pattern = like_pattern(opts.query.as_deref());
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM codeintel_index_jobs j WHERE {LIST_FILTER}"
        ))
        .bind(opts.repo.map(|r| r.get()))
        .bind(&readable)
        .bind(&opts.states)
        .bind(&pattern)
        .bind(&opts.indexers)
        .bind(opts.without_upload)
        .fetch_one(&self.pool)
        .await?;
        let rows = sqlx::query_as(&format!(
            "SELECT j.* FROM codeintel_index_jobs j WHERE {LIST_FILTER}
             ORDER BY j.queued_at DESC, j.id DESC LIMIT $7 OFFSET $8"
        ))
        .bind(opts.repo.map(|r| r.get()))
        .bind(&readable)
        .bind(&opts.states)
        .bind(&pattern)
        .bind(&opts.indexers)
        .bind(opts.without_upload)
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?;
        Ok((rows, total))
    }

    /// The ids of all jobs matching `opts` (`limit` and `offset` are ignored).
    pub async fn matching_ids(&self, opts: &ListIndexJobs) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT j.id FROM codeintel_index_jobs j WHERE {LIST_FILTER} ORDER BY j.id"
        ))
        .bind(opts.repo.map(|r| r.get()))
        .bind(None::<Vec<i32>>)
        .bind(&opts.states)
        .bind(like_pattern(opts.query.as_deref()))
        .bind(&opts.indexers)
        .bind(opts.without_upload)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The position of a queued job in the executor queue (1 = next).
    pub async fn place_in_queue(&self, id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT n FROM (
                SELECT id, row_number() OVER (ORDER BY COALESCE(process_after, queued_at), id) AS n
                FROM codeintel_index_jobs WHERE state = 'queued') q
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Deletes jobs (an executor running one sees it gone at its next heartbeat and stops)
    /// and their job tokens. Returns how many were deleted.
    pub async fn delete(&self, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM executor_job_tokens WHERE queue = 'codeintel' AND job_id = ANY($1)",
        )
        .bind(ids)
        .execute(&mut *tx)
        .await?;
        let n = sqlx::query("DELETE FROM codeintel_index_jobs WHERE id = ANY($1)")
            .bind(ids)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n)
    }

    /// Marks jobs as replaceable: the next scheduling of their commit queues them anew.
    pub async fn mark_should_reindex(&self, ids: &[i64]) -> Result<u64> {
        Ok(
            sqlx::query("UPDATE codeintel_index_jobs SET should_reindex = true WHERE id = ANY($1)")
                .bind(ids)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Whether `commit` of `repo` needs no auto-indexing: an upload not made by an index job
    /// covers it (the repository is indexed some other way, e.g. in CI), or index jobs were
    /// queued for it and none of the latest per root and indexer is marked for reindexing.
    pub async fn is_queued(&self, repo: RepoId, commit: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM codeintel_uploads
                WHERE repository_id = $1 AND commit = $2 AND state NOT IN ('deleting', 'deleted')
                  AND associated_index_id IS NULL AND NOT should_reindex
             ) OR (
                SELECT COALESCE(bool_and(NOT should_reindex), false) FROM (
                    SELECT DISTINCT ON (root, indexer) should_reindex
                    FROM codeintel_index_jobs WHERE repository_id = $1 AND commit = $2
                    ORDER BY root, indexer, queued_at DESC, id DESC) latest
             )",
        )
        .bind(repo.get())
        .bind(commit)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Whether the latest job for (`repo`, `commit`, `root`, `indexer`) exists and is not
    /// marked for reindexing.
    pub async fn is_queued_root_indexer(
        &self,
        repo: RepoId,
        commit: &str,
        root: &str,
        indexer: &str,
    ) -> Result<bool> {
        let v: Option<bool> = sqlx::query_scalar(
            "SELECT NOT should_reindex FROM codeintel_index_jobs
             WHERE repository_id = $1 AND commit = $2 AND root = $3 AND indexer = $4
             ORDER BY queued_at DESC, id DESC LIMIT 1",
        )
        .bind(repo.get())
        .bind(commit)
        .bind(root)
        .bind(indexer)
        .fetch_optional(&self.pool)
        .await?;
        Ok(v.unwrap_or(false))
    }

    /// Records the upload a job produced.
    pub async fn set_associated_upload(&self, id: i64, upload: i64) -> Result<()> {
        sqlx::query("UPDATE codeintel_index_jobs SET associated_upload_id = $2 WHERE id = $1")
            .bind(id)
            .bind(upload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
