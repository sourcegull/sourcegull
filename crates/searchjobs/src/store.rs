//! The search jobs tables (`exhaustive_search_jobs`, `exhaustive_search_repo_jobs`,
//! `exhaustive_search_repo_revision_jobs`): three [`sourcegull_pgqueue`] queues, the
//! aggregated job state (`exhaustive_search_job_state`), listing, progress, cancel and delete.
//!
//! Access checks (creator or site admin) are the caller's: the store reads and writes any
//! job.

use chrono::{DateTime, Utc};
use sourcegull_db::{DbError, Result};
use sourcegull_pgqueue::{QueueConfig, notify_enqueued};
use sourcegull_types::UserId;
use sqlx::PgPool;

/// The queue of jobs: resolves a job's repositories into repo jobs.
pub const JOBS_TABLE: &str = "exhaustive_search_jobs";
/// The queue of repo jobs: resolves revision specifiers into revision jobs.
pub const REPO_JOBS_TABLE: &str = "exhaustive_search_repo_jobs";
/// The queue of revision jobs: searches one repository revision.
pub const REVISION_JOBS_TABLE: &str = "exhaustive_search_repo_revision_jobs";

/// The payload of a job row, for `Queue<JobPayload>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct JobPayload {
    pub initiator_id: UserId,
    pub query: String,
}

/// The payload of a repo job row, for `Queue<RepoJobPayload>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoJobPayload {
    pub search_job_id: i64,
    pub repo_id: i32,
    /// `:`-separated revision specifiers (`HEAD` for the default branch).
    pub ref_spec: String,
}

/// The payload of a revision job row, for `Queue<RevisionJobPayload>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RevisionJobPayload {
    pub search_repo_job_id: i64,
    pub revision: String,
}

/// A search job as users see it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SearchJob {
    pub id: i64,
    pub initiator_id: UserId,
    pub query: String,
    /// The aggregated state: `queued`, `processing`, `completed`, `failed` or `canceled`.
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    /// When the last part finished, once the whole job has.
    pub finished_at: Option<DateTime<Utc>>,
    /// Results written so far.
    pub result_count: i64,
}

impl SearchJob {
    /// Whether nothing of the job runs or waits any more.
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "completed" | "failed" | "canceled")
    }
}

/// Progress over a job's revision jobs (`SearchJob.repoStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepoRevStats {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub in_progress: i64,
}

/// Sort order of [`SearchJobStore::list`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JobOrder {
    Query,
    #[default]
    CreatedAt,
    State,
}

/// Filters and page of [`SearchJobStore::list`].
#[derive(Debug, Clone, Default)]
pub struct ListSearchJobs {
    /// Only jobs whose query contains this text.
    pub query: Option<String>,
    /// Only jobs in these aggregated states (lowercase).
    pub states: Vec<String>,
    /// Only jobs of these creators; `None`: everyone's.
    pub initiators: Option<Vec<UserId>>,
    pub order: JobOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

/// One revision job in the job's log (`/.api/search/export/{id}.log`).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LogRow {
    pub id: i64,
    pub repository: String,
    pub revision: String,
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
}

/// What a revision job needs from its job and repo job.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RevisionContext {
    pub search_job_id: i64,
    pub initiator_id: UserId,
    pub query: String,
    pub repo_id: i32,
    pub repo_name: String,
    pub canceled: bool,
}

/// The job columns of [`SearchJob`], over `exhaustive_search_jobs j`.
const JOB_SELECT: &str = "
    SELECT j.id, j.initiator_id, j.query, j.created_at, j.started_at,
           s.state,
           CASE WHEN s.state IN ('completed', 'failed', 'canceled') THEN greatest(
               j.finished_at,
               (SELECT max(r.finished_at) FROM exhaustive_search_repo_jobs r
                WHERE r.search_job_id = j.id),
               (SELECT max(rr.finished_at) FROM exhaustive_search_repo_revision_jobs rr
                JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
                WHERE r.search_job_id = j.id)
           ) END AS finished_at,
           (SELECT coalesce(sum(rr.result_count), 0)::bigint
            FROM exhaustive_search_repo_revision_jobs rr
            JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
            WHERE r.search_job_id = j.id) AS result_count
    FROM exhaustive_search_jobs j
    CROSS JOIN LATERAL (SELECT exhaustive_search_job_state(j.id) AS state) s";

fn initiator_ids(opts: &ListSearchJobs) -> Option<Vec<i64>> {
    opts.initiators
        .as_ref()
        .map(|v| v.iter().map(|u| u.get()).collect())
}

/// Unfinished queue states.
const LIVE_STATES: &str = "('queued', 'processing', 'errored')";

/// Reads and writes the search jobs tables.
#[derive(Debug, Clone)]
pub struct SearchJobStore {
    pool: PgPool,
}

impl SearchJobStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Queue settings of the jobs queue.
    pub fn jobs_queue_config() -> QueueConfig {
        QueueConfig::new(JOBS_TABLE)
    }

    /// Queue settings of the repo jobs queue.
    pub fn repo_jobs_queue_config() -> QueueConfig {
        QueueConfig::new(REPO_JOBS_TABLE)
    }

    /// Queue settings of the revision jobs queue.
    pub fn revision_jobs_queue_config() -> QueueConfig {
        QueueConfig::new(REVISION_JOBS_TABLE)
    }

    /// Creates a job of `initiator` for `query` and queues it. A user who already has a job
    /// for the same query gets [`DbError::AlreadyExists`].
    pub async fn create(&self, initiator: UserId, query: &str) -> Result<SearchJob> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO exhaustive_search_jobs (initiator_id, query) VALUES ($1, $2) \
             RETURNING id",
        )
        .bind(initiator)
        .bind(query)
        .fetch_one(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, JOBS_TABLE).await?;
        tx.commit().await?;
        self.get(id).await?.ok_or(DbError::NotFound("search job"))
    }

    /// The job with `id`.
    pub async fn get(&self, id: i64) -> Result<Option<SearchJob>> {
        Ok(sqlx::query_as(&format!("{JOB_SELECT} WHERE j.id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The creator of the job with `id`.
    pub async fn initiator(&self, id: i64) -> Result<Option<UserId>> {
        Ok(
            sqlx::query_scalar("SELECT initiator_id FROM exhaustive_search_jobs WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Jobs matching `opts`, one page.
    pub async fn list(&self, opts: &ListSearchJobs) -> Result<Vec<SearchJob>> {
        let order = match opts.order {
            JobOrder::Query => "query",
            JobOrder::CreatedAt => "created_at",
            JobOrder::State => "state",
        };
        let dir = if opts.descending { "DESC" } else { "ASC" };
        Ok(sqlx::query_as(&format!(
            "SELECT * FROM ({JOB_SELECT}) jobs WHERE {} \
             ORDER BY {order} {dir}, id {dir} LIMIT $4 OFFSET $5",
            Self::filter_sql()
        ))
        .bind(opts.query.as_deref().filter(|q| !q.is_empty()))
        .bind(&opts.states)
        .bind(initiator_ids(opts))
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many jobs match the filters of `opts`.
    pub async fn count(&self, opts: &ListSearchJobs) -> Result<i64> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM ({JOB_SELECT}) jobs WHERE {}",
            Self::filter_sql()
        ))
        .bind(opts.query.as_deref().filter(|q| !q.is_empty()))
        .bind(&opts.states)
        .bind(initiator_ids(opts))
        .fetch_one(&self.pool)
        .await?)
    }

    /// `$1` query text, `$2` states, `$3` creators.
    fn filter_sql() -> &'static str {
        "($1::text IS NULL OR strpos(query, $1) > 0)
         AND (cardinality($2::text[]) = 0 OR state = ANY($2))
         AND ($3::bigint[] IS NULL OR initiator_id = ANY($3))"
    }

    /// Cancels everything of the job that has not finished: queued parts at once, running
    /// parts at their worker's next heartbeat. Returns the rows affected.
    pub async fn cancel(&self, id: i64) -> Result<u64> {
        let set = format!(
            "SET cancel = true,
                 state = CASE WHEN state = 'processing' THEN state ELSE 'canceled' END,
                 finished_at = CASE WHEN state = 'processing' THEN finished_at ELSE now() END
             WHERE state IN {LIVE_STATES}"
        );
        let mut tx = self.pool.begin().await?;
        let mut n = sqlx::query(&format!("UPDATE exhaustive_search_jobs {set} AND id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // A job canceled after its split still shows it, even when its parts had finished.
        sqlx::query(
            "UPDATE exhaustive_search_jobs SET cancel = true, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        n += sqlx::query(&format!(
            "UPDATE exhaustive_search_repo_jobs {set} AND search_job_id = $1"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        n += sqlx::query(&format!(
            "UPDATE exhaustive_search_repo_revision_jobs {set} AND search_repo_job_id IN \
             (SELECT id FROM exhaustive_search_repo_jobs WHERE search_job_id = $1)"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(n)
    }

    /// Deletes the job and its parts. Returns whether it existed.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let n = sqlx::query("DELETE FROM exhaustive_search_jobs WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Progress over the job's revision jobs. Waiting, running and retried ones are in
    /// progress; after a cancel nothing is.
    pub async fn repo_rev_stats(&self, id: i64) -> Result<RepoRevStats> {
        let (completed, failed, in_progress, canceled): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE rr.state = 'completed'),
                    count(*) FILTER (WHERE rr.state = 'failed'),
                    count(*) FILTER (WHERE rr.state IN ('queued', 'processing', 'errored')),
                    count(*) FILTER (WHERE rr.state = 'canceled')
             FROM exhaustive_search_repo_revision_jobs rr
             JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
             WHERE r.search_job_id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        let in_progress = if canceled > 0 { 0 } else { in_progress };
        Ok(RepoRevStats {
            total: completed + failed + in_progress,
            completed,
            failed,
            in_progress,
        })
    }

    /// Adds the repo jobs of job `id` (repository, revision specifiers), unless the job was
    /// canceled meanwhile. Returns how many were added.
    pub async fn add_repo_jobs(&self, id: i64, repos: &[(i32, String)]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let mut n = 0;
        for chunk in repos.chunks(1000) {
            let ids: Vec<i32> = chunk.iter().map(|(id, _)| *id).collect();
            let specs: Vec<&str> = chunk.iter().map(|(_, s)| s.as_str()).collect();
            n += sqlx::query(
                "INSERT INTO exhaustive_search_repo_jobs (search_job_id, repo_id, ref_spec)
                 SELECT $1, r.repo_id, r.ref_spec
                 FROM unnest($2::int[], $3::text[]) AS r(repo_id, ref_spec)
                 WHERE NOT EXISTS (SELECT 1 FROM exhaustive_search_jobs
                                   WHERE id = $1 AND cancel)
                 ON CONFLICT (search_job_id, repo_id) DO NOTHING",
            )
            .bind(id)
            .bind(&ids)
            .bind(&specs)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        if n > 0 {
            notify_enqueued(&mut *tx, REPO_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Adds the revision jobs of repo job `id`, unless its job was canceled meanwhile.
    pub async fn add_revision_jobs(&self, id: i64, revisions: &[String]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "INSERT INTO exhaustive_search_repo_revision_jobs (search_repo_job_id, revision)
             SELECT $1, rev FROM unnest($2::text[]) AS rev
             WHERE NOT EXISTS (SELECT 1 FROM exhaustive_search_repo_jobs r
                               JOIN exhaustive_search_jobs j ON j.id = r.search_job_id
                               WHERE r.id = $1 AND (j.cancel OR r.cancel))
             ON CONFLICT (search_repo_job_id, revision) DO NOTHING",
        )
        .bind(id)
        .bind(revisions)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n > 0 {
            notify_enqueued(&mut *tx, REVISION_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// The job of repo job `id`: (job id, creator, query, repository name, canceled).
    pub async fn repo_job_context(
        &self,
        id: i64,
    ) -> Result<Option<(i64, UserId, String, String, bool)>> {
        Ok(sqlx::query_as(
            "SELECT j.id, j.initiator_id, j.query, repo.name, j.cancel
             FROM exhaustive_search_repo_jobs r
             JOIN exhaustive_search_jobs j ON j.id = r.search_job_id
             JOIN repo ON repo.id = r.repo_id
             WHERE r.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The job and repository of revision job `id`.
    pub async fn revision_context(&self, id: i64) -> Result<Option<RevisionContext>> {
        Ok(sqlx::query_as(
            "SELECT j.id AS search_job_id, j.initiator_id, j.query, r.repo_id,
                    repo.name AS repo_name, j.cancel AS canceled
             FROM exhaustive_search_repo_revision_jobs rr
             JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
             JOIN exhaustive_search_jobs j ON j.id = r.search_job_id
             JOIN repo ON repo.id = r.repo_id
             WHERE rr.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Results written by job `job`'s other revision jobs.
    pub async fn results_besides(&self, job: i64, revision_job: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT coalesce(sum(rr.result_count), 0)::bigint
             FROM exhaustive_search_repo_revision_jobs rr
             JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
             WHERE r.search_job_id = $1 AND rr.id <> $2",
        )
        .bind(job)
        .bind(revision_job)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records the results revision job `id` wrote, with a note for the log (shown as its
    /// failure message although it completed, e.g. that the result limit was reached).
    pub async fn set_result_count(&self, id: i64, n: i64, note: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE exhaustive_search_repo_revision_jobs SET result_count = $2,
                 failure_message = $3
             WHERE id = $1",
        )
        .bind(id)
        .bind(n)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The revision jobs of job `id` that wrote results, in order.
    pub async fn result_parts(&self, id: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT rr.id FROM exhaustive_search_repo_revision_jobs rr
             JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
             WHERE r.search_job_id = $1 AND rr.state = 'completed' AND rr.result_count > 0
             ORDER BY rr.id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The revision jobs of job `id` for its log, after `after` (an id), at most `limit`.
    pub async fn log_rows(&self, id: i64, after: i64, limit: i64) -> Result<Vec<LogRow>> {
        Ok(sqlx::query_as(
            "SELECT rr.id, repo.name AS repository, rr.revision, rr.state, rr.started_at,
                    rr.finished_at, rr.failure_message
             FROM exhaustive_search_repo_revision_jobs rr
             JOIN exhaustive_search_repo_jobs r ON r.id = rr.search_repo_job_id
             JOIN repo ON repo.id = r.repo_id
             WHERE r.search_job_id = $1 AND rr.id > $2
             ORDER BY rr.id LIMIT $3",
        )
        .bind(id)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Finished jobs created before `before` (the retention janitor's), at most `limit`.
    pub async fn expired(&self, before: DateTime<Utc>, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM exhaustive_search_jobs
             WHERE created_at < $1
               AND exhaustive_search_job_state(id) IN ('completed', 'failed', 'canceled')
             ORDER BY id LIMIT $2",
        )
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Which of `ids` are jobs.
    pub async fn existing(&self, ids: &[i64]) -> Result<Vec<i64>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM exhaustive_search_jobs WHERE id = ANY($1)")
                .bind(ids)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}
