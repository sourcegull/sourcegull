//! `access_tokens` store. Only the SHA-256 of a token is stored.

use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{AccessTokenId, UserId};
use sqlx::PgPool;

const COLUMNS: &str = "id, subject_user_id, creator_user_id, prefix, note, scopes, created_at, \
     last_used_at, expires_at";

/// [`COLUMNS`] of `access_tokens t`, for queries that join other tables.
const T_COLUMNS: &str = "t.id, t.subject_user_id, t.creator_user_id, t.prefix, t.note, t.scopes, \
     t.created_at, t.last_used_at, t.expires_at";

/// Sort order for [`AccessTokenStore::list`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessTokenOrder {
    /// By creation time.
    #[default]
    CreatedAt,
    /// By the subject's username (case-insensitive), then newest first.
    Username,
    /// By last use; never-used tokens count as the oldest.
    LastUsedAt,
    /// By expiry; tokens that never expire count as the latest.
    ExpiresAt,
}

/// Which tokens [`AccessTokenStore::list`] returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccessTokenState {
    /// Every token that is not deleted, expired ones included.
    #[default]
    All,
    /// Tokens past their expiry.
    Expired,
    /// Unexpired tokens not used (or, if never used, created) in the last `unused_days`.
    Unused,
}

/// Filters for [`AccessTokenStore::list`] and [`AccessTokenStore::count_listed`]. Tokens of
/// deactivated users are never listed.
#[derive(Debug, Clone)]
pub struct ListAccessTokensOptions {
    /// Case-insensitive substring of the subject's username or the note.
    pub query: Option<String>,
    pub state: AccessTokenState,
    /// The idle period of [`AccessTokenState::Unused`].
    pub unused_days: i32,
    pub order: AccessTokenOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListAccessTokensOptions {
    fn default() -> Self {
        Self {
            query: None,
            state: AccessTokenState::All,
            unused_days: 90,
            order: AccessTokenOrder::CreatedAt,
            descending: true,
            limit: 100,
            offset: 0,
        }
    }
}

/// Token totals from [`AccessTokenStore::counts`], over users who are not deactivated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccessTokenCounts {
    /// Tokens that are not deleted, expired ones included.
    pub total: i64,
    /// Tokens past their expiry.
    pub expired: i64,
    /// Unexpired tokens idle for the `unused_days` given.
    pub unused: i64,
}

/// The `FROM` and `WHERE` of [`AccessTokenStore::list`] and [`AccessTokenStore::count_listed`]:
/// $1 name or note pattern, $2 state (`all`, `expired`, `unused`), $3 idle days.
const LIST_FROM: &str = "access_tokens t JOIN users u ON u.id = t.subject_user_id
    WHERE t.deleted_at IS NULL AND u.deleted_at IS NULL
      AND ($1::text IS NULL OR u.username ILIKE $1 OR t.note ILIKE $1)
      AND CASE $2::text
            WHEN 'expired' THEN t.expires_at <= now()
            WHEN 'unused' THEN (t.expires_at IS NULL OR t.expires_at > now())
              AND coalesce(t.last_used_at, t.created_at) < now() - make_interval(days => $3)
            ELSE true
          END";

fn state_name(state: AccessTokenState) -> &'static str {
    match state {
        AccessTokenState::All => "all",
        AccessTokenState::Expired => "expired",
        AccessTokenState::Unused => "unused",
    }
}

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

    /// One page of every user's tokens, filtered and sorted by `opts`.
    pub async fn list(&self, opts: &ListAccessTokensOptions) -> Result<Vec<AccessToken>> {
        let dir = if opts.descending { "DESC" } else { "ASC" };
        // Never used counts as oldest, never expires as latest; ties fall back to the id.
        let order = match opts.order {
            AccessTokenOrder::CreatedAt => format!("t.created_at {dir}, t.id {dir}"),
            AccessTokenOrder::Username => {
                format!("lower(u.username) {dir}, t.created_at DESC, t.id DESC")
            }
            AccessTokenOrder::LastUsedAt => format!(
                "t.last_used_at {dir} NULLS {}, t.id {dir}",
                if opts.descending { "LAST" } else { "FIRST" }
            ),
            AccessTokenOrder::ExpiresAt => format!(
                "t.expires_at {dir} NULLS {}, t.id {dir}",
                if opts.descending { "FIRST" } else { "LAST" }
            ),
        };
        Ok(sqlx::query_as(&format!(
            "SELECT {T_COLUMNS} FROM {LIST_FROM} ORDER BY {order} LIMIT $4 OFFSET $5"
        ))
        .bind(list_pattern(opts))
        .bind(state_name(opts.state))
        .bind(opts.unused_days)
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Number of tokens [`Self::list`] lists for `opts`, over all pages.
    pub async fn count_listed(&self, opts: &ListAccessTokensOptions) -> Result<i64> {
        Ok(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {LIST_FROM}"))
                .bind(list_pattern(opts))
                .bind(state_name(opts.state))
                .bind(opts.unused_days)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Totals of every user's tokens; `unused` counts tokens idle for `unused_days`.
    pub async fn counts(&self, unused_days: i32) -> Result<AccessTokenCounts> {
        let (total, expired, unused): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*),
                    count(*) FILTER (WHERE t.expires_at <= now()),
                    count(*) FILTER (WHERE (t.expires_at IS NULL OR t.expires_at > now())
                      AND coalesce(t.last_used_at, t.created_at) < now() - make_interval(days => $1))
             FROM access_tokens t JOIN users u ON u.id = t.subject_user_id
             WHERE t.deleted_at IS NULL AND u.deleted_at IS NULL",
        )
        .bind(unused_days)
        .fetch_one(&self.pool)
        .await?;
        Ok(AccessTokenCounts {
            total,
            expired,
            unused,
        })
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

fn list_pattern(opts: &ListAccessTokensOptions) -> Option<String> {
    opts.query
        .as_deref()
        .map(|q| format!("%{}%", super::users::escape_like(q)))
}
