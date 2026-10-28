//! `repo_update_jobs` store: requests to clone or fetch a repo.
//!
//! # Contract with the repo node
//!
//! - The table is a [`sourcegull_pgqueue`] queue with two payload columns: `repo_id` and
//!   `priority` (higher first, see [`RepoUpdatePriority`]), plus a free-form `reason`.
//! - Producers (discovery, webhooks, admins) call [`RepoUpdateJobStore::enqueue`]: at most
//!   one row per repo is `queued`; enqueueing again raises that row's priority (and clears any
//!   retry delay) instead of adding a row. A row being processed does not block a new queued
//!   row, so a push that lands during a fetch is fetched again afterwards. Every enqueue sends
//!   `NOTIFY queue_repo_update_jobs` on commit.
//! - Consumers dequeue with `Queue::<RepoUpdateJob>::new(pool,
//!   RepoUpdateJobStore::queue_config())` (priority ordering), or with their own `FOR UPDATE
//!   SKIP LOCKED` query when they must filter (e.g. only repos assigned to this node), keeping
//!   the standard state transitions. Whether a job means "clone" or "fetch" follows from
//!   `gitserver_repos.clone_status`.

use crate::Result;
use chrono::{DateTime, Utc};
use sourcegull_pgqueue::{QueueConfig, notify_enqueued};
use sourcegull_types::{RepoId, RepoUpdateJobId};
use sqlx::{PgConnection, PgPool};

/// The queue table.
pub const REPO_UPDATE_JOBS_TABLE: &str = "repo_update_jobs";

/// Priorities of repo update requests (`repo_update_jobs.priority`); higher runs first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepoUpdatePriority(pub i16);

impl RepoUpdatePriority {
    /// Periodic background refresh.
    pub const LOW: Self = Self(0);
    /// A newly discovered repo's first clone.
    pub const NORMAL: Self = Self(10);
    /// Discovery saw `last_activity_at` move forward.
    pub const HIGH: Self = Self(20);
    /// A push webhook or an explicit user/admin request: fetch now.
    pub const IMMEDIATE: Self = Self(30);
}

/// Why an update was requested (`repo_update_jobs.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoUpdateReason {
    /// Discovery found a new (or returning) repo.
    Discovered,
    /// Discovery saw new activity on the code host.
    Activity,
    /// A push or tag push webhook.
    Webhook,
    /// A user or admin asked for it.
    Manual,
    /// A periodic refresh.
    Scheduled,
    /// A site admin asked for a fresh clone (`recloneRepository`). Sticks to the queued
    /// request until it runs, whatever is enqueued for the repo meanwhile.
    Reclone,
}

impl RepoUpdateReason {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Activity => "activity",
            Self::Webhook => "webhook",
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::Reclone => "reclone",
        }
    }
}

/// The payload of a repo update job, for `Queue<RepoUpdateJob>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoUpdateJob {
    pub repo_id: RepoId,
    pub priority: i16,
    pub reason: String,
}

/// A repo update job as listed for a repo.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoUpdateJobRecord {
    pub id: RepoUpdateJobId,
    pub repo_id: RepoId,
    pub state: String,
    pub priority: i16,
    pub reason: String,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
}

/// Queues and lists repo update requests.
#[derive(Debug, Clone)]
pub struct RepoUpdateJobStore {
    pool: PgPool,
}

impl RepoUpdateJobStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The queue settings consumers use for this table (priority ordering).
    pub fn queue_config() -> QueueConfig {
        QueueConfig::new(REPO_UPDATE_JOBS_TABLE).with_priority()
    }

    /// Requests a clone/fetch of `repo` (coalesced; see the module docs). Returns the id of
    /// the queued row.
    pub async fn enqueue(
        &self,
        repo: RepoId,
        priority: RepoUpdatePriority,
        reason: RepoUpdateReason,
    ) -> Result<RepoUpdateJobId> {
        let mut tx = self.pool.begin().await?;
        let id = enqueue_update(&mut tx, repo, priority, reason).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The newest jobs of a repo, newest first.
    pub async fn list_for_repo(
        &self,
        repo: RepoId,
        limit: i64,
    ) -> Result<Vec<RepoUpdateJobRecord>> {
        Ok(sqlx::query_as(
            "SELECT id, repo_id, state, priority, reason, queued_at, started_at, finished_at,
                    failure_message
             FROM repo_update_jobs WHERE repo_id = $1 ORDER BY id DESC LIMIT $2",
        )
        .bind(repo)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Number of queued jobs (the clone/fetch queue length gauge).
    pub async fn queued_count(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM repo_update_jobs WHERE state = 'queued'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Deletes finished jobs older than `before`. Returns the number deleted.
    pub async fn delete_finished_before(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM repo_update_jobs
             WHERE state IN ('completed', 'failed', 'canceled') AND finished_at < $1",
        )
        .bind(before)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

/// Inserts or raises the queued request for `repo` in `tx` and notifies consumers on commit.
pub(crate) async fn enqueue_update(
    tx: &mut PgConnection,
    repo: RepoId,
    priority: RepoUpdatePriority,
    reason: RepoUpdateReason,
) -> Result<RepoUpdateJobId> {
    let id: RepoUpdateJobId = sqlx::query_scalar(
        "INSERT INTO repo_update_jobs (repo_id, priority, reason) VALUES ($1, $2, $3)
         ON CONFLICT (repo_id) WHERE state = 'queued' DO UPDATE SET
             priority = greatest(repo_update_jobs.priority, EXCLUDED.priority),
             reason = CASE WHEN repo_update_jobs.reason = 'reclone' THEN 'reclone'
                           WHEN EXCLUDED.reason = 'reclone'
                             OR EXCLUDED.priority > repo_update_jobs.priority
                           THEN EXCLUDED.reason ELSE repo_update_jobs.reason END,
             process_after = NULL
         RETURNING id",
    )
    .bind(repo)
    .bind(priority.0)
    .bind(reason.as_str())
    .fetch_one(&mut *tx)
    .await?;
    notify_enqueued(&mut *tx, REPO_UPDATE_JOBS_TABLE).await?;
    Ok(id)
}
