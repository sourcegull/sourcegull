//! `repo_ref_events` and `webhook_logs` stores.

use crate::Result;
use crate::notify::{channels, notify};
use chrono::{DateTime, Utc};
use sourcegull_types::{ExternalServiceId, RepoId};
use sqlx::PgPool;

/// Where a ref event came from (`repo_ref_events.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefEventSource {
    /// The code host announced it (unverified until fetched).
    Webhook,
    /// A fetch on the repo node moved the ref.
    Fetch,
}

impl RefEventSource {
    /// The stored form.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Fetch => "fetch",
        }
    }
}

/// One ref change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    /// Full ref name, e.g. `refs/heads/main`.
    pub name: String,
    /// `None` when the ref was created.
    pub old_oid: Option<String>,
    /// `None` when the ref was deleted.
    pub new_oid: Option<String>,
}

/// A recorded ref change.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RefEvent {
    pub id: i64,
    pub repo_id: RepoId,
    #[sqlx(rename = "ref")]
    pub ref_name: String,
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub source: String,
    pub at: DateTime<Utc>,
}

/// Writes and reads `repo_ref_events`.
#[derive(Debug, Clone)]
pub struct RefEventStore {
    pool: PgPool,
}

impl RefEventStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records ref changes of a repo and sends `NOTIFY repo_ref_events` (payload: the repo id)
    /// on commit. Returns the new event ids.
    pub async fn record(
        &self,
        repo: RepoId,
        updates: &[RefUpdate],
        source: RefEventSource,
    ) -> Result<Vec<i64>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        let mut ids = Vec::with_capacity(updates.len());
        for u in updates {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO repo_ref_events (repo_id, ref, old_oid, new_oid, source)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(repo)
            .bind(&u.name)
            .bind(&u.old_oid)
            .bind(&u.new_oid)
            .bind(source.as_str())
            .fetch_one(&mut *tx)
            .await?;
            ids.push(id);
        }
        notify(&mut *tx, channels::REPO_REF_EVENTS, &repo.to_string()).await?;
        tx.commit().await?;
        Ok(ids)
    }

    /// Events with an id greater than `after`, oldest first (a consumer's cursor scan).
    pub async fn list_after(&self, after: i64, limit: i64) -> Result<Vec<RefEvent>> {
        Ok(sqlx::query_as(
            "SELECT id, repo_id, ref, old_oid, new_oid, source, at FROM repo_ref_events
             WHERE id > $1 ORDER BY id LIMIT $2",
        )
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Deletes events older than `before`. Returns the number deleted.
    pub async fn delete_before(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query("DELETE FROM repo_ref_events WHERE at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

/// A webhook delivery to log.
#[derive(Debug, Clone, Default)]
pub struct NewWebhookLog {
    pub external_service_id: Option<ExternalServiceId>,
    pub event: Option<String>,
    pub status_code: i32,
    /// Selected headers and the body, without secrets.
    pub request: serde_json::Value,
    pub error: Option<String>,
}

/// A logged webhook delivery.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct WebhookLog {
    pub id: i64,
    pub received_at: DateTime<Utc>,
    pub external_service_id: Option<ExternalServiceId>,
    pub event: Option<String>,
    pub status_code: i32,
    pub request: serde_json::Value,
    pub error: Option<String>,
}

/// Which webhook deliveries to list.
#[derive(Debug, Clone, Default)]
pub struct WebhookLogFilter {
    /// Only deliveries matched to this connection.
    pub service: Option<ExternalServiceId>,
    /// Only deliveries matched to no connection.
    pub only_unmatched: bool,
    /// Only deliveries answered with a status of 400 or more.
    pub only_errors: bool,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// Only deliveries with a smaller id (the pagination cursor).
    pub before_id: Option<i64>,
}

/// Writes and reads `webhook_logs`.
#[derive(Debug, Clone)]
pub struct WebhookLogStore {
    pool: PgPool,
}

impl WebhookLogStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Logs a delivery.
    pub async fn insert(&self, log: &NewWebhookLog) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO webhook_logs (external_service_id, event, status_code, request, error)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(log.external_service_id)
        .bind(&log.event)
        .bind(log.status_code)
        .bind(&log.request)
        .bind(&log.error)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The newest deliveries, newest first, optionally for one connection.
    pub async fn list(
        &self,
        service: Option<ExternalServiceId>,
        limit: i64,
    ) -> Result<Vec<WebhookLog>> {
        Ok(sqlx::query_as(
            "SELECT id, received_at, external_service_id, event, status_code, request, error
             FROM webhook_logs WHERE ($1::bigint IS NULL OR external_service_id = $1)
             ORDER BY id DESC LIMIT $2",
        )
        .bind(service)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Up to `limit` deliveries matching `filter`, newest first.
    pub async fn search(&self, filter: &WebhookLogFilter, limit: i64) -> Result<Vec<WebhookLog>> {
        Ok(sqlx::query_as(
            "SELECT id, received_at, external_service_id, event, status_code, request, error
             FROM webhook_logs
             WHERE ($1::bigint IS NULL OR external_service_id = $1)
               AND (NOT $2 OR external_service_id IS NULL)
               AND (NOT $3 OR status_code >= 400)
               AND ($4::timestamptz IS NULL OR received_at >= $4)
               AND ($5::timestamptz IS NULL OR received_at <= $5)
               AND ($6::bigint IS NULL OR id < $6)
             ORDER BY id DESC LIMIT $7",
        )
        .bind(filter.service)
        .bind(filter.only_unmatched)
        .bind(filter.only_errors)
        .bind(filter.since)
        .bind(filter.until)
        .bind(filter.before_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of deliveries matching `filter` (ignoring `before_id`).
    pub async fn count(&self, filter: &WebhookLogFilter) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM webhook_logs
             WHERE ($1::bigint IS NULL OR external_service_id = $1)
               AND (NOT $2 OR external_service_id IS NULL)
               AND (NOT $3 OR status_code >= 400)
               AND ($4::timestamptz IS NULL OR received_at >= $4)
               AND ($5::timestamptz IS NULL OR received_at <= $5)",
        )
        .bind(filter.service)
        .bind(filter.only_unmatched)
        .bind(filter.only_errors)
        .bind(filter.since)
        .bind(filter.until)
        .fetch_one(&self.pool)
        .await?)
    }

    /// One delivery.
    pub async fn get(&self, id: i64) -> Result<Option<WebhookLog>> {
        Ok(sqlx::query_as(
            "SELECT id, received_at, external_service_id, event, status_code, request, error
             FROM webhook_logs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Deletes deliveries older than `before`. Returns the number deleted.
    pub async fn delete_before(&self, before: DateTime<Utc>) -> Result<u64> {
        let r = sqlx::query("DELETE FROM webhook_logs WHERE received_at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
