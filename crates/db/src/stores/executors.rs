//! `executor_heartbeats` and `executor_job_tokens`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_types::RepoId;
use sqlx::PgPool;
use sqlx::types::Json;

use crate::Result;

/// A job an executor reported running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveJob {
    pub queue: String,
    pub id: i64,
}

/// What an executor reports about itself with each heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutorInfo {
    pub hostname: String,
    pub queue_names: Vec<String>,
    pub os: String,
    pub architecture: String,
    pub executor_version: String,
    pub runtime: String,
    pub runtime_version: String,
    pub git_version: String,
}

/// An executor row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Executor {
    pub id: i64,
    pub hostname: String,
    pub queue_names: Vec<String>,
    pub os: String,
    pub architecture: String,
    pub executor_version: String,
    pub runtime: String,
    pub runtime_version: String,
    pub git_version: String,
    #[sqlx(json)]
    pub active_jobs: Vec<ActiveJob>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// The job a job token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTokenScope {
    pub queue: String,
    pub job_id: i64,
    pub repo_id: RepoId,
}

/// Store for executors and job tokens.
#[derive(Debug, Clone)]
pub struct ExecutorStore {
    pool: PgPool,
}

impl ExecutorStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records a heartbeat: upserts the executor by hostname with its active jobs.
    pub async fn heartbeat(&self, info: &ExecutorInfo, active: &[ActiveJob]) -> Result<()> {
        sqlx::query(
            "INSERT INTO executor_heartbeats (hostname, queue_names, os, architecture,
                executor_version, runtime, runtime_version, git_version, active_jobs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (hostname) DO UPDATE SET
                queue_names = EXCLUDED.queue_names, os = EXCLUDED.os,
                architecture = EXCLUDED.architecture,
                executor_version = EXCLUDED.executor_version, runtime = EXCLUDED.runtime,
                runtime_version = EXCLUDED.runtime_version, git_version = EXCLUDED.git_version,
                active_jobs = EXCLUDED.active_jobs, last_seen_at = now()",
        )
        .bind(&info.hostname)
        .bind(&info.queue_names)
        .bind(&info.os)
        .bind(&info.architecture)
        .bind(&info.executor_version)
        .bind(&info.runtime)
        .bind(&info.runtime_version)
        .bind(&info.git_version)
        .bind(Json(active))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Executors seen since `since`, most recently seen first.
    pub async fn list(&self, since: Option<DateTime<Utc>>) -> Result<Vec<Executor>> {
        Ok(sqlx::query_as(
            "SELECT * FROM executor_heartbeats
             WHERE $1::timestamptz IS NULL OR last_seen_at >= $1
             ORDER BY last_seen_at DESC, id",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Deletes executors not seen since `before`.
    pub async fn delete_stale(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM executor_heartbeats WHERE last_seen_at < $1")
                .bind(before)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Stores the token of a job (replacing an earlier one for the same job).
    pub async fn create_job_token(&self, scope: &JobTokenScope, sha256: &[u8; 32]) -> Result<()> {
        sqlx::query(
            "INSERT INTO executor_job_tokens (queue, job_id, repository_id, token_sha256)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (queue, job_id) DO UPDATE SET
                repository_id = EXCLUDED.repository_id,
                token_sha256 = EXCLUDED.token_sha256, created_at = now()",
        )
        .bind(&scope.queue)
        .bind(scope.job_id)
        .bind(scope.repo_id.get())
        .bind(&sha256[..])
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The job a token hash belongs to.
    pub async fn job_token(&self, sha256: &[u8; 32]) -> Result<Option<JobTokenScope>> {
        let row: Option<(String, i64, i32)> = sqlx::query_as(
            "SELECT queue, job_id, repository_id FROM executor_job_tokens
             WHERE token_sha256 = $1",
        )
        .bind(&sha256[..])
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(queue, job_id, repo)| JobTokenScope {
            queue,
            job_id,
            repo_id: RepoId(repo),
        }))
    }

    /// Deletes the token of a job.
    pub async fn delete_job_token(&self, queue: &str, job_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM executor_job_tokens WHERE queue = $1 AND job_id = $2")
            .bind(queue)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes tokens of `queue` whose job is no longer processing in `table` (a trusted
    /// queue table name).
    pub async fn delete_finished_job_tokens(&self, queue: &str, table: &str) -> Result<u64> {
        sourcegull_pgqueue::validate_table_name(table)?;
        Ok(sqlx::query(&format!(
            "DELETE FROM executor_job_tokens t WHERE t.queue = $1 AND NOT EXISTS (
                SELECT 1 FROM {table} j WHERE j.id = t.job_id AND j.state = 'processing')"
        ))
        .bind(queue)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
