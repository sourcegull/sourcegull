//! `sessions` store: server-side session rows keyed by the SHA-256 of the
//! cookie value. Caching lives in the authn crate.

use crate::Result;
use chrono::{DateTime, Utc};
use sourcegull_types::UserId;
use sqlx::PgPool;

/// A stored session.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct SessionRow {
    /// The signed-in user (`None` for an anonymous session).
    pub user_id: Option<UserId>,
    /// Session-specific data.
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Reads and writes `sessions`.
#[derive(Debug, Clone)]
pub struct SessionRowStore {
    pool: PgPool,
}

impl SessionRowStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts a session and returns its creation time.
    pub async fn insert(
        &self,
        id_hash: &[u8; 32],
        user_id: Option<UserId>,
        expires_at: DateTime<Utc>,
        data: &serde_json::Value,
    ) -> Result<DateTime<Utc>> {
        Ok(sqlx::query_scalar(
            "INSERT INTO sessions (id_hash, user_id, expires_at, data)
             VALUES ($1, $2, $3, $4) RETURNING created_at",
        )
        .bind(&id_hash[..])
        .bind(user_id)
        .bind(expires_at)
        .bind(data)
        .fetch_one(&self.pool)
        .await?)
    }

    /// A non-expired session by id hash.
    pub async fn get(&self, id_hash: &[u8; 32]) -> Result<Option<SessionRow>> {
        Ok(sqlx::query_as(
            "SELECT user_id, data, created_at, last_active_at, expires_at FROM sessions
             WHERE id_hash = $1 AND expires_at > now()",
        )
        .bind(&id_hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Deletes a session. Returns whether it existed.
    pub async fn delete(&self, id_hash: &[u8; 32]) -> Result<bool> {
        let r = sqlx::query("DELETE FROM sessions WHERE id_hash = $1")
            .bind(&id_hash[..])
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Deletes all sessions of a user. Returns how many were deleted.
    pub async fn delete_for_user(&self, user_id: UserId) -> Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Deletes all sessions of a user except the one with `keep`. Returns how many were deleted.
    pub async fn delete_for_user_except(&self, user_id: UserId, keep: &[u8; 32]) -> Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id_hash <> $2")
            .bind(user_id)
            .bind(&keep[..])
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Deletes expired sessions (a periodic janitor job). Returns how many.
    pub async fn delete_expired(&self) -> Result<u64> {
        let r = sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
