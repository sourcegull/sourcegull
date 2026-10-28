//! `batch_spec_workspace_files`: files a server-side batch
//! spec's steps `mount`, uploaded with the spec and downloaded by executors.
//!
//! A file is identified by its spec, its directory (`path`, relative to the bulk change spec's
//! directory, `''` at its root) and its name; uploading the same path and name again replaces
//! the content. Files go with their spec ([`BatchSpecStore::delete`](super::BatchSpecStore)
//! cascades).

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use sourcegull_db::{Db, Result};

/// A stored file, without its content.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BatchSpecWorkspaceFile {
    pub id: i64,
    /// The id used in GraphQL IDs and download URLs.
    pub rand_id: String,
    pub batch_spec_id: i64,
    /// The directory, relative to the bulk change spec's directory (`''` at its root).
    pub path: String,
    pub filename: String,
    /// Size of the content in bytes.
    pub size: i64,
    /// The file's modification time on the uploader's machine.
    pub modified_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BatchSpecWorkspaceFile {
    /// `path/filename`, or `filename` at the root.
    pub fn full_path(&self) -> String {
        if self.path.is_empty() {
            self.filename.clone()
        } else {
            format!("{}/{}", self.path, self.filename)
        }
    }
}

/// A file to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBatchSpecWorkspaceFile<'a> {
    pub batch_spec_id: i64,
    pub path: &'a str,
    pub filename: &'a str,
    pub content: &'a [u8],
    pub modified_at: DateTime<Utc>,
}

const COLUMNS: &str =
    "id, rand_id, batch_spec_id, path, filename, size, modified_at, created_at, updated_at";

/// Store for `batch_spec_workspace_files`.
#[derive(Debug, Clone)]
pub struct BatchSpecWorkspaceFileStore {
    pool: PgPool,
}

impl BatchSpecWorkspaceFileStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Stores a file, replacing the content of one with the same spec, path and name (which
    /// keeps its id).
    pub async fn upsert(
        &self,
        f: &NewBatchSpecWorkspaceFile<'_>,
    ) -> Result<BatchSpecWorkspaceFile> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO batch_spec_workspace_files
                 (batch_spec_id, path, filename, size, content, modified_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (batch_spec_id, path, filename) DO UPDATE SET
                 size = EXCLUDED.size, content = EXCLUDED.content,
                 modified_at = EXCLUDED.modified_at, updated_at = now()
             RETURNING {COLUMNS}"
        ))
        .bind(f.batch_spec_id)
        .bind(f.path)
        .bind(f.filename)
        .bind(i64::try_from(f.content.len()).unwrap_or(i64::MAX))
        .bind(f.content)
        .bind(f.modified_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The spec's files ordered by path and name, skipping `offset`; all of them when `limit`
    /// is 0.
    pub async fn list(
        &self,
        batch_spec_id: i64,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<BatchSpecWorkspaceFile>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM batch_spec_workspace_files WHERE batch_spec_id = $1
             ORDER BY path, filename OFFSET $2 LIMIT $3"
        ))
        .bind(batch_spec_id)
        .bind(offset.max(0))
        .bind(if limit > 0 { limit } else { i64::MAX })
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many files the spec has.
    pub async fn count(&self, batch_spec_id: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM batch_spec_workspace_files WHERE batch_spec_id = $1",
        )
        .bind(batch_spec_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// A file of the spec by its random id.
    pub async fn get_by_rand_id(
        &self,
        batch_spec_id: i64,
        rand_id: &str,
    ) -> Result<Option<BatchSpecWorkspaceFile>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM batch_spec_workspace_files
             WHERE batch_spec_id = $1 AND rand_id = $2"
        ))
        .bind(batch_spec_id)
        .bind(rand_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// A file of the spec by its directory and name.
    pub async fn get_by_name(
        &self,
        batch_spec_id: i64,
        path: &str,
        filename: &str,
    ) -> Result<Option<BatchSpecWorkspaceFile>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM batch_spec_workspace_files
             WHERE batch_spec_id = $1 AND path = $2 AND filename = $3"
        ))
        .bind(batch_spec_id)
        .bind(path)
        .bind(filename)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// A file's content.
    pub async fn content(&self, id: i64) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT content FROM batch_spec_workspace_files WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Deletes every file of the spec; returns how many there were.
    pub async fn delete_for_spec(&self, batch_spec_id: i64) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM batch_spec_workspace_files WHERE batch_spec_id = $1")
                .bind(batch_spec_id)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}
