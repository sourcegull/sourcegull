//! Access requests (`auth.accessRequest`): applications from visitors when sign-up is
//! closed, decided by site admins, and the single-use account setup links of approved ones.

use chrono::{DateTime, Utc};
use sourcegull_types::UserId;
use sqlx::PgPool;

use crate::Result;

/// How long an account setup link stays valid.
pub const INVITE_TTL_HOURS: i32 = 24;

/// The state of an access request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessRequestStatus {
    Pending,
    Approved,
    Rejected,
}

impl AccessRequestStatus {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Approved => "APPROVED",
            Self::Rejected => "REJECTED",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "APPROVED" => Self::Approved,
            "REJECTED" => Self::Rejected,
            _ => Self::Pending,
        }
    }
}

/// One access request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRequest {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub additional_info: String,
    pub status: AccessRequestStatus,
    pub decision_by_user_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    name: String,
    email: String,
    additional_info: String,
    status: String,
    decision_by_user_id: Option<UserId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<Row> for AccessRequest {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            name: r.name,
            email: r.email,
            additional_info: r.additional_info,
            status: AccessRequestStatus::parse(&r.status),
            decision_by_user_id: r.decision_by_user_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const COLUMNS: &str =
    "id, name, email, additional_info, status, decision_by_user_id, created_at, updated_at";

/// Reads and writes `access_requests` and `access_request_invites`.
#[derive(Debug, Clone)]
pub struct AccessRequestStore {
    pool: PgPool,
}

impl AccessRequestStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records a request unless the email already has one or belongs to a user. Returns whether
    /// a request was recorded (callers answer the same either way, so that the form does not
    /// reveal which emails are known).
    pub async fn create(&self, name: &str, email: &str, additional_info: &str) -> Result<bool> {
        let done = sqlx::query(
            "INSERT INTO access_requests (name, email, additional_info)
             SELECT $1, $2, $3 WHERE NOT EXISTS
                 (SELECT 1 FROM user_emails WHERE lower(email) = lower($2))
             ON CONFLICT DO NOTHING",
        )
        .bind(name)
        .bind(email)
        .bind(additional_info)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// The request with `id`.
    pub async fn get(&self, id: i64) -> Result<Option<AccessRequest>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM access_requests WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Up to `limit` requests (optionally in `status`) with an id above `after`, oldest first.
    pub async fn list(
        &self,
        status: Option<AccessRequestStatus>,
        after: i64,
        limit: i64,
    ) -> Result<Vec<AccessRequest>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM access_requests
             WHERE ($1::text IS NULL OR status = $1) AND id > $2 ORDER BY id LIMIT $3"
        ))
        .bind(status.map(AccessRequestStatus::as_str))
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The number of requests, optionally in `status`.
    pub async fn count(&self, status: Option<AccessRequestStatus>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM access_requests WHERE ($1::text IS NULL OR status = $1)",
        )
        .bind(status.map(AccessRequestStatus::as_str))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Moves a request from `from` to `to`, recording `by` (`None`: undoing a decision).
    /// Returns the updated request, or `None` when it does not exist or is not in `from`, so
    /// that two admins cannot both decide one request.
    pub async fn transition(
        &self,
        id: i64,
        from: AccessRequestStatus,
        to: AccessRequestStatus,
        by: Option<UserId>,
    ) -> Result<Option<AccessRequest>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "UPDATE access_requests SET status = $3, decision_by_user_id = $4, updated_at = now()
             WHERE id = $1 AND status = $2 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(from.as_str())
        .bind(to.as_str())
        .bind(by)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Stores an account setup link for `user`, by the SHA-256 hash of its token.
    pub async fn create_invite(&self, user: UserId, token_hash: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO access_request_invites (token_hash, user_id, expires_at)
             VALUES ($1, $2, now() + make_interval(hours => $3))",
        )
        .bind(token_hash)
        .bind(user)
        .bind(INVITE_TTL_HOURS)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Uses up the unexpired, unused link with `token_hash`, returning its live user.
    pub async fn redeem_invite(&self, token_hash: &[u8]) -> Result<Option<UserId>> {
        Ok(sqlx::query_scalar(
            "UPDATE access_request_invites i SET used_at = now()
             FROM users u
             WHERE i.token_hash = $1 AND i.used_at IS NULL AND i.expires_at > now()
               AND u.id = i.user_id AND u.deleted_at IS NULL
             RETURNING i.user_id",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?)
    }
}
