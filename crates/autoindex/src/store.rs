//! Auto-indexing settings and scheduler bookkeeping: per-repository index
//! configuration, the site's inference script, when the sweep last scanned a repository,
//! the per-repository rate limit and the ref event cursor.

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_types::RepoId;
use sqlx::PgPool;

/// A repository's index configuration as stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct IndexConfigurationRow {
    pub repository_id: RepoId,
    pub data: String,
    pub updated_at: DateTime<Utc>,
}

/// Store for `codeintel_index_configuration`, `codeintel_inference_scripts`,
/// `codeintel_autoindex_repos` and `codeintel_autoindex_cursor`.
#[derive(Debug, Clone)]
pub struct AutoIndexStore {
    pool: PgPool,
}

impl AutoIndexStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The index configuration of `repo`, if one was saved.
    pub async fn index_configuration(&self, repo: RepoId) -> Result<Option<IndexConfigurationRow>> {
        Ok(sqlx::query_as(
            "SELECT repository_id, data, updated_at FROM codeintel_index_configuration
             WHERE repository_id = $1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Saves the index configuration of `repo`; empty `data` removes it.
    pub async fn set_index_configuration(&self, repo: RepoId, data: &str) -> Result<()> {
        if data.trim().is_empty() {
            sqlx::query("DELETE FROM codeintel_index_configuration WHERE repository_id = $1")
                .bind(repo)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO codeintel_index_configuration (repository_id, data) VALUES ($1, $2)
             ON CONFLICT (repository_id) DO UPDATE SET data = $2, updated_at = now()",
        )
        .bind(repo)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The site's inference script (`""` when none is set).
    pub async fn inference_script(&self) -> Result<String> {
        let s: Option<String> =
            sqlx::query_scalar("SELECT script FROM codeintel_inference_scripts")
                .fetch_optional(&self.pool)
                .await?;
        Ok(s.unwrap_or_default())
    }

    /// Replaces the site's inference script; empty removes it.
    pub async fn set_inference_script(&self, script: &str) -> Result<()> {
        if script.trim().is_empty() {
            sqlx::query("DELETE FROM codeintel_inference_scripts")
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO codeintel_inference_scripts (script) VALUES ($1)
             ON CONFLICT (id) DO UPDATE SET script = $1, updated_at = now()",
        )
        .bind(script)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Of `candidates`, those the sweep scanned before `before` (or never), least recently
    /// scanned first, at most `limit`.
    pub async fn repos_to_scan(
        &self,
        candidates: &[RepoId],
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<RepoId>> {
        let ids: Vec<i32> = candidates.iter().map(|r| r.get()).collect();
        Ok(sqlx::query_scalar(
            "SELECT r.id FROM unnest($1::int[]) AS r(id)
             LEFT JOIN codeintel_autoindex_repos s ON s.repository_id = r.id
             WHERE s.scanned_at IS NULL OR s.scanned_at < $2
             ORDER BY s.scanned_at NULLS FIRST, r.id LIMIT $3",
        )
        .bind(&ids)
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(RepoId)
        .collect())
    }

    /// Records that the sweep scanned `repo` at `at`.
    pub async fn mark_scanned(&self, repo: RepoId, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "INSERT INTO codeintel_autoindex_repos (repository_id, scanned_at) VALUES ($1, $2)
             ON CONFLICT (repository_id) DO UPDATE SET scanned_at = $2",
        )
        .bind(repo)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// When the sweep last scanned `repo`.
    pub async fn scanned_at(&self, repo: RepoId) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar(
            "SELECT scanned_at FROM codeintel_autoindex_repos WHERE repository_id = $1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?
        .flatten())
    }

    /// Takes one commit from `repo`'s rate limit: at most `limit` commits per `window`.
    /// Returns false (and takes nothing) when the window is used up.
    pub async fn take_rate_limit(
        &self,
        repo: RepoId,
        limit: i32,
        window: chrono::TimeDelta,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let start = now - window;
        let taken: Option<i32> = sqlx::query_scalar(
            "INSERT INTO codeintel_autoindex_repos AS s
                (repository_id, window_started_at, window_commits)
             VALUES ($1, $2, 1)
             ON CONFLICT (repository_id) DO UPDATE SET
                window_started_at = CASE WHEN s.window_started_at <= $3 THEN $2
                    ELSE s.window_started_at END,
                window_commits = CASE WHEN s.window_started_at <= $3 THEN 1
                    ELSE s.window_commits + 1 END
             WHERE s.window_started_at <= $3 OR s.window_commits < $4
             RETURNING s.window_commits",
        )
        .bind(repo)
        .bind(now)
        .bind(start)
        .bind(limit)
        .fetch_optional(&self.pool)
        .await?;
        Ok(taken.is_some())
    }

    /// Cloned, undeleted repositories (id, name), for the sweep to match policies against.
    pub async fn cloned_repos(&self) -> Result<Vec<(RepoId, String)>> {
        Ok(sqlx::query_as(
            "SELECT r.id, r.name FROM repo r JOIN gitserver_repos g ON g.repo_id = r.id
             WHERE r.deleted_at IS NULL AND g.clone_status = 'cloned' ORDER BY r.id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The id of the last ref event the scheduler handled.
    pub async fn cursor(&self) -> Result<i64> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT last_event_id FROM codeintel_autoindex_cursor")
                .fetch_optional(&self.pool)
                .await?;
        Ok(v.unwrap_or(0))
    }

    pub async fn set_cursor(&self, last_event_id: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO codeintel_autoindex_cursor (last_event_id) VALUES ($1)
             ON CONFLICT (id) DO UPDATE SET last_event_id = $1",
        )
        .bind(last_event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
