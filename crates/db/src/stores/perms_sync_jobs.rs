//! `permission_sync_jobs` store: the code host permissions sync queue and its history.
//!
//! The table is a [`sourcegull_pgqueue`] queue dequeued by priority (see [`PermsSyncReason`]). A
//! leader-elected scheduler calls [`PermsSyncJobStore::enqueue_due_users`] and
//! [`PermsSyncJobStore::enqueue_due_repos`] (oldest sync first); sign-in, admins ("sync now")
//! and webhooks call [`PermsSyncJobStore::enqueue`]. At most one row per user and per
//! repository is queued; queueing again raises its priority. Nothing is queued while no code
//! host connection has an `authorization` block.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sourcegull_pgqueue::{QueueConfig, notify_enqueued};
use sourcegull_types::{ExternalServiceId, RepoId, UserId};
use sqlx::PgPool;

use crate::Result;

/// The queue table.
pub const PERMS_SYNC_JOBS_TABLE: &str = "permission_sync_jobs";

/// Why a permissions sync was queued; also its priority (higher runs first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermsSyncReason {
    /// The user signed in (their new OAuth token may see more).
    UserSignedIn,
    /// The repository is new.
    RepoCreated,
    /// An admin asked for it.
    Manual,
    /// A code host event announced a membership change.
    Webhook,
    /// The scheduler refreshes the oldest syncs.
    Scheduled,
}

impl PermsSyncReason {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserSignedIn => "user_signed_in",
            Self::RepoCreated => "repo_created",
            Self::Manual => "manual",
            Self::Webhook => "webhook",
            Self::Scheduled => "scheduled",
        }
    }

    /// The queue priority: sign-in > new repo > manual > webhook > scheduled.
    pub const fn priority(self) -> i16 {
        match self {
            Self::UserSignedIn => 50,
            Self::RepoCreated => 40,
            Self::Manual => 30,
            Self::Webhook => 20,
            Self::Scheduled => 10,
        }
    }
}

/// Whose permissions a job syncs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermsSyncSubject {
    /// User-centric: the repositories the user can read.
    User(UserId),
    /// Repo-centric: the users who can read the repository.
    Repo(RepoId),
}

/// The payload of a job, for `Queue<PermsSyncJob>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PermsSyncJob {
    pub user_id: Option<UserId>,
    pub repository_id: Option<RepoId>,
    pub reason: String,
}

impl PermsSyncJob {
    /// The subject (the table guarantees exactly one of the ids).
    pub fn subject(&self) -> Option<PermsSyncSubject> {
        match (self.user_id, self.repository_id) {
            (Some(u), None) => Some(PermsSyncSubject::User(u)),
            (None, Some(r)) => Some(PermsSyncSubject::Repo(r)),
            _ => None,
        }
    }
}

/// What a run did, recorded on its row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermsSyncResult {
    /// Grants added.
    pub added: i32,
    /// Grants removed.
    pub removed: i32,
    /// Repositories (user-centric) or users (repo-centric) the code hosts returned.
    pub found: i32,
    /// One entry per connection: `{externalServiceID, status, message}`.
    pub code_host_states: Value,
}

/// One run as shown in the sync history.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct PermsSyncJobRecord {
    pub id: i64,
    pub state: String,
    pub reason: String,
    pub priority: i16,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub failure_message: Option<String>,
    pub cancel: bool,
    pub user_id: Option<UserId>,
    pub repository_id: Option<RepoId>,
    pub triggered_by_user_id: Option<UserId>,
    pub added: i32,
    pub removed: i32,
    pub found: i32,
    pub code_host_states: Value,
}

/// Filters for [`PermsSyncJobStore::list`].
#[derive(Debug, Clone, Default)]
pub struct ListPermsSyncJobs {
    pub subject: Option<PermsSyncSubject>,
    /// Only jobs in this state (`queued`, `processing`, `completed`, `failed`, `canceled`).
    pub state: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

const RECORD_COLUMNS: &str = "id, state, reason, priority, queued_at, started_at, finished_at, \
     failure_message, cancel, user_id, repository_id, triggered_by_user_id, added, removed, \
     found, code_host_states";

/// Whether some live connection has an `authorization` block (otherwise nothing is synced).
const ANY_AUTHZ_CONNECTION: &str =
    "EXISTS (SELECT 1 FROM external_services WHERE deleted_at IS NULL AND NOT unrestricted)";

/// Reads and writes `permission_sync_jobs` and `user_permissions_sync`.
#[derive(Debug, Clone)]
pub struct PermsSyncJobStore {
    pool: PgPool,
}

impl PermsSyncJobStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The queue settings workers use for this table.
    pub fn queue_config() -> QueueConfig {
        QueueConfig::new(PERMS_SYNC_JOBS_TABLE).with_priority()
    }

    /// Queues a sync of `subject` unless no connection has an `authorization` block. A queued
    /// job for the subject gets the higher of the two priorities. Returns the job id.
    pub async fn enqueue(
        &self,
        subject: PermsSyncSubject,
        reason: PermsSyncReason,
        triggered_by: Option<UserId>,
    ) -> Result<Option<i64>> {
        let (user, repo, target) = match subject {
            PermsSyncSubject::User(u) => (
                Some(u),
                None,
                "(user_id) WHERE state = 'queued' AND user_id IS NOT NULL",
            ),
            PermsSyncSubject::Repo(r) => (
                None,
                Some(r),
                "(repository_id) WHERE state = 'queued' AND repository_id IS NOT NULL",
            ),
        };
        let mut tx = self.pool.begin().await?;
        let id: Option<i64> = sqlx::query_scalar(&format!(
            "INSERT INTO permission_sync_jobs
                 (user_id, repository_id, reason, priority, triggered_by_user_id)
             SELECT $1, $2, $3, $4, $5 WHERE {ANY_AUTHZ_CONNECTION}
             ON CONFLICT {target} DO UPDATE SET
                 priority = greatest(permission_sync_jobs.priority, EXCLUDED.priority),
                 reason = CASE WHEN EXCLUDED.priority > permission_sync_jobs.priority
                               THEN EXCLUDED.reason ELSE permission_sync_jobs.reason END,
                 triggered_by_user_id = coalesce(EXCLUDED.triggered_by_user_id,
                     permission_sync_jobs.triggered_by_user_id),
                 process_after = NULL
             RETURNING id"
        ))
        .bind(user)
        .bind(repo)
        .bind(reason.as_str())
        .bind(reason.priority())
        .bind(triggered_by)
        .fetch_optional(&mut *tx)
        .await?;
        if id.is_some() {
            notify_enqueued(&mut *tx, PERMS_SYNC_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Queues scheduled syncs for up to `limit` live users whose last sync is older than
    /// `older_than` (never-synced users first, then the oldest) and who have no queued or
    /// running job. Returns the users queued.
    pub async fn enqueue_due_users(
        &self,
        limit: i64,
        older_than: DateTime<Utc>,
    ) -> Result<Vec<UserId>> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<UserId> = sqlx::query_scalar(&format!(
            "INSERT INTO permission_sync_jobs (user_id, reason, priority)
             SELECT u.id, $3, $4 FROM users u
             LEFT JOIN user_permissions_sync s ON s.user_id = u.id
             WHERE u.deleted_at IS NULL AND {ANY_AUTHZ_CONNECTION}
               AND (s.synced_at IS NULL OR s.synced_at < $2)
               AND NOT EXISTS (SELECT 1 FROM permission_sync_jobs j
                   WHERE j.user_id = u.id AND j.state IN ('queued', 'processing'))
             ORDER BY s.synced_at ASC NULLS FIRST, u.id
             LIMIT $1
             ON CONFLICT DO NOTHING
             RETURNING user_id"
        ))
        .bind(limit)
        .bind(older_than)
        .bind(PermsSyncReason::Scheduled.as_str())
        .bind(PermsSyncReason::Scheduled.priority())
        .fetch_all(&mut *tx)
        .await?;
        if !ids.is_empty() {
            notify_enqueued(&mut *tx, PERMS_SYNC_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Queues scheduled repo-centric syncs for up to `limit` live private repositories listed
    /// by one of `services`, synced before `older_than` (never-synced first, so new
    /// repositories go first), without a queued or running job. Returns the repos queued.
    pub async fn enqueue_due_repos(
        &self,
        services: &[ExternalServiceId],
        limit: i64,
        older_than: DateTime<Utc>,
    ) -> Result<Vec<RepoId>> {
        if services.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        let ids: Vec<RepoId> = sqlx::query_scalar(
            "INSERT INTO permission_sync_jobs (repository_id, reason, priority)
             SELECT r.id, $4, $5 FROM repo r
             LEFT JOIN repo_permissions p ON p.repo_id = r.id
             WHERE r.deleted_at IS NULL AND r.private
               AND EXISTS (SELECT 1 FROM external_service_repos esr
                   WHERE esr.repo_id = r.id AND esr.external_service_id = ANY($1))
               AND (p.synced_at IS NULL OR p.synced_at < $3)
               AND NOT EXISTS (SELECT 1 FROM permission_sync_jobs j
                   WHERE j.repository_id = r.id AND j.state IN ('queued', 'processing'))
             ORDER BY p.synced_at ASC NULLS FIRST, r.id
             LIMIT $2
             ON CONFLICT DO NOTHING
             RETURNING repository_id",
        )
        .bind(services)
        .bind(limit)
        .bind(older_than)
        .bind(PermsSyncReason::Scheduled.as_str())
        .bind(PermsSyncReason::Scheduled.priority())
        .fetch_all(&mut *tx)
        .await?;
        if !ids.is_empty() {
            notify_enqueued(&mut *tx, PERMS_SYNC_JOBS_TABLE).await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Records a run's result on its row.
    pub async fn record_result(&self, job: i64, result: &PermsSyncResult) -> Result<()> {
        sqlx::query(
            "UPDATE permission_sync_jobs
             SET added = $2, removed = $3, found = $4, code_host_states = $5 WHERE id = $1",
        )
        .bind(job)
        .bind(result.added)
        .bind(result.removed)
        .bind(result.found)
        .bind(&result.code_host_states)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records that `user`'s permissions were just synced.
    pub async fn mark_user_synced(&self, user: UserId) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_permissions_sync (user_id) VALUES ($1)
             ON CONFLICT (user_id) DO UPDATE SET synced_at = now()",
        )
        .bind(user)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When `user`'s permissions were last synced.
    pub async fn user_synced_at(&self, user: UserId) -> Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("SELECT synced_at FROM user_permissions_sync WHERE user_id = $1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Whether a sync of `user` is queued or running that may change what they can see right
    /// now: one queued by their sign-in, or any while they have never been synced.
    pub async fn user_sync_in_progress(&self, user: UserId) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM permission_sync_jobs
                  WHERE user_id = $1 AND state IN ('queued', 'processing')
                    AND (reason = 'user_signed_in'
                         OR NOT EXISTS (SELECT 1 FROM user_permissions_sync WHERE user_id = $1)))",
        )
        .bind(user)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Runs matching `filter`, newest first.
    pub async fn list(&self, filter: &ListPermsSyncJobs) -> Result<Vec<PermsSyncJobRecord>> {
        let (user, repo) = subject_ids(filter.subject);
        Ok(sqlx::query_as(&format!(
            "SELECT {RECORD_COLUMNS} FROM permission_sync_jobs
             WHERE ($1::bigint IS NULL OR user_id = $1)
               AND ($2::integer IS NULL OR repository_id = $2)
               AND ($3::text IS NULL OR state = $3)
             ORDER BY id DESC LIMIT $4 OFFSET $5"
        ))
        .bind(user)
        .bind(repo)
        .bind(filter.state.as_deref())
        .bind(filter.limit)
        .bind(filter.offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of runs matching `filter` (ignoring limit and offset).
    pub async fn count(&self, filter: &ListPermsSyncJobs) -> Result<i64> {
        let (user, repo) = subject_ids(filter.subject);
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM permission_sync_jobs
             WHERE ($1::bigint IS NULL OR user_id = $1)
               AND ($2::integer IS NULL OR repository_id = $2)
               AND ($3::text IS NULL OR state = $3)",
        )
        .bind(user)
        .bind(repo)
        .bind(filter.state.as_deref())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Requests cancellation of a queued or running job. Returns whether it was found.
    pub async fn cancel(&self, job: i64) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE permission_sync_jobs SET cancel = true
             WHERE id = $1 AND state IN ('queued', 'processing')",
        )
        .bind(job)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Deletes finished runs beyond the newest `keep` per user and per repository
    /// (`permissions.syncJobsHistorySize`). Returns the number deleted.
    pub async fn prune_history(&self, keep: i64) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM permission_sync_jobs j USING (
                 SELECT id, row_number() OVER (
                     PARTITION BY user_id, repository_id ORDER BY id DESC) AS n
                 FROM permission_sync_jobs
                 WHERE state IN ('completed', 'failed', 'canceled')) ranked
             WHERE j.id = ranked.id AND ranked.n > $1",
        )
        .bind(keep.max(1))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

fn subject_ids(subject: Option<PermsSyncSubject>) -> (Option<UserId>, Option<RepoId>) {
    match subject {
        Some(PermsSyncSubject::User(u)) => (Some(u), None),
        Some(PermsSyncSubject::Repo(r)) => (None, Some(r)),
        None => (None, None),
    }
}
