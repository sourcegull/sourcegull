//! `access_tokens` store. Only the SHA-256 of a token is stored.

use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{AccessTokenId, UserId};
use sqlx::PgPool;

const COLUMNS: &str = "id, subject_user_id, creator_user_id, prefix, note, scopes, created_at, \
     last_used_at, expires_at";

/// Fields for [`AccessTokenStore::create`].
#[derive(Debug, Clone)]
pub struct NewAccessToken<'a> {
    /// The user the token acts as.
    pub subject_user_id: UserId,
    /// The user who created it (a site admin may create tokens for others).
    pub creator_user_id: UserId,
    /// SHA-256 of the plaintext token.
    pub hash: &'a [u8; 32],
    /// Non-secret display prefix.
    pub prefix: &'a str,
    /// Scopes, e.g. `user:all`.
    pub scopes: &'a [String],
    /// Free-form note.
    pub note: &'a str,
    /// Expiry; `None` means it never expires.
    pub expires_at: Option<DateTime<Utc>>,
}

/// The fields needed to authenticate with a token.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TokenRecord {
    pub id: AccessTokenId,
    pub subject_user_id: UserId,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A token as listed to its owner or an admin (never includes the secret).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AccessToken {
    pub id: AccessTokenId,
    pub subject_user_id: UserId,
    pub creator_user_id: UserId,
    pub prefix: String,
    pub note: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Reads and writes `access_tokens`. Deletion is soft (`deleted_at`).
#[derive(Debug, Clone)]
pub struct AccessTokenStore {
    pool: PgPool,
}

impl AccessTokenStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts a token and returns its id.
    pub async fn create(&self, new: &NewAccessToken<'_>) -> Result<AccessTokenId> {
        Ok(sqlx::query_scalar(
            "INSERT INTO access_tokens
                 (subject_user_id, creator_user_id, value_sha256, prefix, scopes, note, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(new.subject_user_id)
        .bind(new.creator_user_id)
        .bind(&new.hash[..])
        .bind(new.prefix)
        .bind(new.scopes)
        .bind(new.note)
        .bind(new.expires_at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Finds a live (not deleted, not expired) token by hash whose subject is
    /// a live user.
    pub async fn lookup_by_hash(&self, hash: &[u8; 32]) -> Result<Option<TokenRecord>> {
        Ok(sqlx::query_as(
            "SELECT t.id, t.subject_user_id, t.scopes, t.expires_at
             FROM access_tokens t JOIN users u ON u.id = t.subject_user_id
             WHERE t.value_sha256 = $1 AND t.deleted_at IS NULL
               AND (t.expires_at IS NULL OR t.expires_at > now())
               AND u.deleted_at IS NULL",
        )
        .bind(&hash[..])
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records use of a token. Debounced: only writes when the stored value is
    /// unset or older than one minute. Returns whether a row was updated.
    pub async fn touch_last_used(&self, id: AccessTokenId) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE access_tokens SET last_used_at = now()
             WHERE id = $1 AND deleted_at IS NULL
               AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// A non-deleted token by id (expired ones included).
    pub async fn get(&self, id: AccessTokenId) -> Result<Option<AccessToken>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM access_tokens WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Non-deleted tokens whose subject is `user` (expired ones included),
    /// newest first.
    pub async fn list_for_user(&self, user: UserId) -> Result<Vec<AccessToken>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM access_tokens
             WHERE subject_user_id = $1 AND deleted_at IS NULL
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Soft-deletes a token. With `subject = Some(user)`, only a token of that
    /// user is deleted. Fails with [`DbError::NotFound`] if nothing matched.
    pub async fn delete(&self, id: AccessTokenId, subject: Option<UserId>) -> Result<()> {
        let r = sqlx::query(
            "UPDATE access_tokens SET deleted_at = now()
             WHERE id = $1 AND deleted_at IS NULL
               AND ($2::bigint IS NULL OR subject_user_id = $2)",
        )
        .bind(id)
        .bind(subject)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("access token"));
        }
        Ok(())
    }

    /// Soft-deletes every token whose subject is `user`. Returns how many were deleted.
    pub async fn delete_for_user(&self, user: UserId) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE access_tokens SET deleted_at = now()
             WHERE subject_user_id = $1 AND deleted_at IS NULL",
        )
        .bind(user)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Number of non-deleted tokens, optionally of one subject user.
    pub async fn count(&self, subject: Option<UserId>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM access_tokens
             WHERE deleted_at IS NULL AND ($1::bigint IS NULL OR subject_user_id = $1)",
        )
        .bind(subject)
        .fetch_one(&self.pool)
        .await?)
    }
}
