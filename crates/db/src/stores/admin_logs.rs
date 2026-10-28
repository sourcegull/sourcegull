//! The outbound request log and the slow request log, both bounded rings in
//! Postgres so every web process shows the requests of every process.
//!
//! - `outbound_requests`: code host API calls, the newest `outboundRequestLogLimit` kept
//!   (writers pass the limit to [`OutboundRequestStore::insert_many`]). Rows arrive redacted.
//! - `slow_requests`: GraphQL requests and searches slower than their thresholds, the newest
//!   [`SLOW_REQUESTS_KEPT`] kept.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::Result;

/// Slow requests kept.
pub const SLOW_REQUESTS_KEPT: i64 = 1024;

/// An outbound request to log (already redacted).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewOutboundRequest {
    pub started_at: DateTime<Utc>,
    pub method: String,
    pub url: String,
    /// `{name: [values]}`.
    pub request_headers: Value,
    pub request_body: String,
    /// 0 when no response arrived.
    pub status_code: i32,
    /// `{name: [values]}`.
    pub response_headers: Value,
    pub duration_ms: i64,
    pub error_message: String,
    /// Which client made the request (e.g. the code host connection).
    pub creation_stack_frame: String,
    /// Where in Sourcegull the request was made.
    pub call_stack: String,
}

/// A logged outbound request.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct OutboundRequestLogItem {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub method: String,
    pub url: String,
    pub request_headers: Value,
    pub request_body: String,
    pub status_code: i32,
    pub response_headers: Value,
    pub duration_ms: i64,
    pub error_message: String,
    pub creation_stack_frame: String,
    pub call_stack: String,
}

/// Writes and reads `outbound_requests`.
#[derive(Debug, Clone)]
pub struct OutboundRequestStore {
    pool: PgPool,
}

const OUTBOUND_COLUMNS: &str = "id, started_at, method, url, request_headers, request_body, \
    status_code, response_headers, duration_ms, error_message, creation_stack_frame, call_stack";

impl OutboundRequestStore {
    /// A store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Logs `items` and keeps only the newest `keep` rows (`keep <= 0` deletes everything).
    pub async fn insert_many(&self, items: &[NewOutboundRequest], keep: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if keep > 0 {
            for r in items {
                sqlx::query(
                    "INSERT INTO outbound_requests (started_at, method, url, request_headers,
                         request_body, status_code, response_headers, duration_ms, error_message,
                         creation_stack_frame, call_stack)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                )
                .bind(r.started_at)
                .bind(&r.method)
                .bind(&r.url)
                .bind(&r.request_headers)
                .bind(&r.request_body)
                .bind(r.status_code)
                .bind(&r.response_headers)
                .bind(r.duration_ms)
                .bind(&r.error_message)
                .bind(&r.creation_stack_frame)
                .bind(&r.call_stack)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "DELETE FROM outbound_requests WHERE id <= (
                 SELECT id FROM outbound_requests ORDER BY id DESC OFFSET $1 LIMIT 1)",
        )
        .bind(keep.max(0))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Up to `limit` requests older than `before` (an id; `None` for the newest), newest first.
    pub async fn list(
        &self,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<OutboundRequestLogItem>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {OUTBOUND_COLUMNS} FROM outbound_requests
             WHERE ($1::bigint IS NULL OR id < $1) ORDER BY id DESC LIMIT $2"
        ))
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// One request.
    pub async fn get(&self, id: i64) -> Result<Option<OutboundRequestLogItem>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {OUTBOUND_COLUMNS} FROM outbound_requests WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The number of logged requests.
    pub async fn count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM outbound_requests")
            .fetch_one(&self.pool)
            .await?)
    }
}

/// What kind of request was slow (`slow_requests.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowRequestKind {
    /// A GraphQL request (`observability.logSlowGraphQLRequests`).
    GraphQl,
    /// A search (`observability.logSlowSearches`).
    Search,
}

impl SlowRequestKind {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GraphQl => "graphql",
            Self::Search => "search",
        }
    }
}

/// A slow request to log.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSlowRequest {
    pub started_at: DateTime<Utc>,
    pub duration_ms: f64,
    pub kind: SlowRequestKind,
    pub user_id: Option<i64>,
    /// The GraphQL operation name, or `search`.
    pub name: String,
    /// Where the request came from (e.g. `browser`, `other`).
    pub source: String,
    pub repository: Option<String>,
    /// The GraphQL variables as JSON.
    pub variables: String,
    pub errors: Vec<String>,
    /// The GraphQL document or the search query.
    pub query: String,
    pub filepath: Option<String>,
}

/// A logged slow request.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct SlowRequestLogItem {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub duration_ms: f64,
    pub kind: String,
    pub user_id: Option<i64>,
    pub name: String,
    pub source: String,
    pub repository: Option<String>,
    pub variables: String,
    pub errors: Value,
    pub query: String,
    pub filepath: Option<String>,
}

/// Writes and reads `slow_requests`.
#[derive(Debug, Clone)]
pub struct SlowRequestStore {
    pool: PgPool,
}

impl SlowRequestStore {
    /// A store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Logs `r` and keeps the newest [`SLOW_REQUESTS_KEPT`] rows.
    pub async fn insert(&self, r: &NewSlowRequest) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO slow_requests (started_at, duration_ms, kind, user_id, name, source,
                 repository, variables, errors, query, filepath)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
        )
        .bind(r.started_at)
        .bind(r.duration_ms)
        .bind(r.kind.as_str())
        .bind(r.user_id)
        .bind(&r.name)
        .bind(&r.source)
        .bind(&r.repository)
        .bind(&r.variables)
        .bind(serde_json::json!(r.errors))
        .bind(&r.query)
        .bind(&r.filepath)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM slow_requests WHERE id <= (
                 SELECT id FROM slow_requests ORDER BY id DESC OFFSET $1 LIMIT 1)",
        )
        .bind(SLOW_REQUESTS_KEPT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Up to `limit` requests older than `before` (an id), newest first, optionally of one
    /// kind.
    pub async fn list(
        &self,
        kind: Option<SlowRequestKind>,
        before: Option<i64>,
        limit: i64,
    ) -> Result<Vec<SlowRequestLogItem>> {
        Ok(sqlx::query_as(
            "SELECT id, started_at, duration_ms, kind, user_id, name, source, repository,
                    variables, errors, query, filepath
             FROM slow_requests
             WHERE ($1::text IS NULL OR kind = $1) AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(kind.map(SlowRequestKind::as_str))
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of logged requests, optionally of one kind.
    pub async fn count(&self, kind: Option<SlowRequestKind>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM slow_requests WHERE ($1::text IS NULL OR kind = $1)",
        )
        .bind(kind.map(SlowRequestKind::as_str))
        .fetch_one(&self.pool)
        .await?)
    }
}
