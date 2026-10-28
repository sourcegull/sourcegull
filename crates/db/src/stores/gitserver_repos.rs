//! `gitserver_repos` store: the repo node's view of each repo.
//!
//! Discovery creates the row (`not_cloned`) when it inserts a repo; afterwards only the repo
//! node writes it, through the methods below.

use super::search_index::OWNED_BY_NODE;
use crate::Result;
use sourcegull_types::{CloneStatus, GitserverRepo, RemoteUnavailable, RepoId};
use sqlx::PgPool;

const COLUMNS: &str = "repo_id, clone_status, cloning_progress, last_fetched, last_changed, \
     last_error, repo_size_bytes, head_oid, node, corrupted_at, updated_at, remote_unavailable";

/// The result of a successful clone or fetch, for [`GitserverRepoStore::record_fetched`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchResult {
    /// Whether any ref changed (sets `last_changed`).
    pub changed: bool,
    /// What `HEAD` resolves to now.
    pub head_oid: Option<String>,
    /// Size of the bare repo on disk.
    pub size_bytes: Option<i64>,
    /// The repo node that holds the repo.
    pub node: String,
    /// The code host has no git repository the node can fetch (none yet, disabled, or no
    /// code access for the token); the local copy is empty.
    pub remote_unavailable: Option<RemoteUnavailable>,
}

/// The repo node's fetch scheduling state of a repo, the inputs of
/// `sourcegull_gitstore::schedule::next_fetch`.
#[derive(Debug, Clone, Default, PartialEq, sqlx::FromRow)]
pub struct FetchSchedule {
    /// When the last fetch (or clone) finished successfully.
    pub last_fetched: Option<chrono::DateTime<chrono::Utc>>,
    /// When the last fetch changed any ref.
    pub last_changed: Option<chrono::DateTime<chrono::Utc>>,
    /// The last attempt, successful or not.
    pub last_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Failed attempts since the last success.
    pub consecutive_failures: i32,
    /// Moving average of the time between changes, in seconds.
    pub mean_change_interval_secs: Option<f64>,
    /// When the next periodic fetch is due.
    pub next_fetch_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reads and writes `gitserver_repos`.
#[derive(Debug, Clone)]
pub struct GitserverRepoStore {
    pool: PgPool,
}

impl GitserverRepoStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The row of a repo, if it exists.
    pub async fn get(&self, repo: RepoId) -> Result<Option<GitserverRepo>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM gitserver_repos WHERE repo_id = $1"
        ))
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The rows of several repos (missing ones are skipped).
    pub async fn get_many(&self, repos: &[RepoId]) -> Result<Vec<GitserverRepo>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM gitserver_repos WHERE repo_id = ANY($1)"
        ))
        .bind(repos)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks a clone as started on `node`. Returns `false`, writing nothing, when the repo was
    /// deleted or is assigned to another node.
    pub async fn set_cloning(&self, repo: RepoId, node: &str) -> Result<bool> {
        let written = sqlx::query(&format!(
            "INSERT INTO gitserver_repos (repo_id, clone_status, node, cloning_progress)
             SELECT $1, 'cloning', $2, NULL WHERE {OWNED_BY_NODE}
             ON CONFLICT (repo_id) DO UPDATE SET clone_status = 'cloning', node = $2,
                 cloning_progress = NULL, updated_at = now()"
        ))
        .bind(repo)
        .bind(node)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Updates the progress line of a running clone.
    pub async fn set_cloning_progress(&self, repo: RepoId, progress: &str) -> Result<()> {
        sqlx::query(
            "UPDATE gitserver_repos SET cloning_progress = $2, updated_at = now() WHERE repo_id = $1",
        )
        .bind(repo)
        .bind(progress)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records a successful clone or fetch: `cloned`, `last_fetched = now()`, `last_changed`
    /// when refs moved, the error and corruption marker cleared, and `remote_unavailable` as
    /// reported (a project without a fetchable repository is an empty clone, not an error).
    /// Nothing is recorded, and `false` returned, when the repository was deleted or is
    /// assigned to another node: a fetch that ends after the repository moved must not
    /// describe the new owner's copy.
    pub async fn record_fetched(&self, repo: RepoId, result: &FetchResult) -> Result<bool> {
        sqlx::query("INSERT INTO gitserver_repos (repo_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(repo)
            .execute(&self.pool)
            .await?;
        let written = sqlx::query(
            "UPDATE gitserver_repos SET clone_status = 'cloned', cloning_progress = NULL,
                 last_fetched = now(),
                 last_changed = CASE WHEN $2 OR last_changed IS NULL THEN now() ELSE last_changed END,
                 last_error = NULL, corrupted_at = NULL,
                 head_oid = coalesce($3, head_oid),
                 repo_size_bytes = coalesce($4, repo_size_bytes),
                 node = $5, remote_unavailable = $6, updated_at = now()
             WHERE repo_id = $1
               AND EXISTS (SELECT 1 FROM repo WHERE id = $1
                   AND (shard IS NULL OR shard = $5) FOR SHARE)",
        )
        .bind(repo)
        .bind(result.changed)
        .bind(&result.head_oid)
        .bind(result.size_bytes)
        .bind(&result.node)
        .bind(result.remote_unavailable)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Records a failed clone or fetch by `node`. A failed initial clone goes back to
    /// `not_cloned`; a failed fetch of a cloned repo keeps it `cloned`. Nothing is recorded, and
    /// `false` returned, when the repository is assigned to another node.
    pub async fn record_error(&self, repo: RepoId, node: &str, error: &str) -> Result<bool> {
        let written = sqlx::query(&format!(
            "UPDATE gitserver_repos SET last_error = $3, cloning_progress = NULL,
                 clone_status = CASE WHEN clone_status = 'cloning' THEN 'not_cloned'
                                     ELSE clone_status END,
                 updated_at = now()
             WHERE repo_id = $1 AND {OWNED_BY_NODE}"
        ))
        .bind(repo)
        .bind(node)
        .bind(error)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Puts a clone by `node` that was cancelled back to `not_cloned`, unless the repository is
    /// assigned to another node by now.
    pub async fn reset_cloning(&self, repo: RepoId, node: &str) -> Result<bool> {
        let written = sqlx::query(&format!(
            "UPDATE gitserver_repos SET clone_status = 'not_cloned', cloning_progress = NULL,
                 updated_at = now()
             WHERE repo_id = $1 AND clone_status = 'cloning' AND {OWNED_BY_NODE}"
        ))
        .bind(repo)
        .bind(node)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Sets the clone status directly (e.g. `not_cloned` after the repo was removed from disk).
    pub async fn set_clone_status(&self, repo: RepoId, status: CloneStatus) -> Result<()> {
        sqlx::query(
            "UPDATE gitserver_repos SET clone_status = $2, updated_at = now() WHERE repo_id = $1",
        )
        .bind(repo)
        .bind(status.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The fetch scheduling state of a repo, if it has a row.
    pub async fn fetch_schedule(&self, repo: RepoId) -> Result<Option<FetchSchedule>> {
        Ok(sqlx::query_as(
            "SELECT last_fetched, last_changed, last_attempt_at, consecutive_failures,
                    mean_change_interval_secs, next_fetch_at
             FROM gitserver_repos WHERE repo_id = $1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Stores the scheduling fields of `schedule` (`last_attempt_at`, `consecutive_failures`,
    /// `mean_change_interval_secs`, `next_fetch_at`); the fetch timestamps are left alone, they
    /// are written by [`record_fetched`](Self::record_fetched).
    pub async fn set_fetch_schedule(&self, repo: RepoId, schedule: &FetchSchedule) -> Result<()> {
        sqlx::query(
            "UPDATE gitserver_repos SET last_attempt_at = $2, consecutive_failures = $3,
                 mean_change_interval_secs = $4, next_fetch_at = $5
             WHERE repo_id = $1",
        )
        .bind(repo)
        .bind(schedule.last_attempt_at)
        .bind(schedule.consecutive_failures)
        .bind(schedule.mean_change_interval_secs)
        .bind(schedule.next_fetch_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets only `next_fetch_at` (the scheduler found the repo not due yet).
    pub async fn set_next_fetch_at(
        &self,
        repo: RepoId,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        sqlx::query("UPDATE gitserver_repos SET next_fetch_at = $2 WHERE repo_id = $1")
            .bind(repo)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Marks the repo corrupt (the repo node will re-clone it).
    pub async fn set_corrupted(&self, repo: RepoId, reason: &str) -> Result<()> {
        sqlx::query(
            "UPDATE gitserver_repos SET corrupted_at = now(), last_error = $2, updated_at = now()
             WHERE repo_id = $1",
        )
        .bind(repo)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
