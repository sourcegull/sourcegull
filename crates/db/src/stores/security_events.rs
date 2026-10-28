//! `security_event_logs` store: the audit log.

use crate::Result;
use chrono::{DateTime, Utc};
use sourcegull_types::UserId;
use sqlx::PgPool;

/// A security event to record.
#[derive(Debug, Clone, Default)]
pub struct NewSecurityEvent {
    /// Event name, e.g. `SignInSucceeded`, `AccessTokenCreated`.
    pub name: String,
    pub url: String,
    pub user_id: Option<UserId>,
    pub anonymous_user_id: String,
    /// Where it came from, e.g. `BACKEND`.
    pub source: String,
    /// Event-specific details.
    pub argument: serde_json::Value,
    /// Product version that wrote it.
    pub version: String,
}

/// A recorded security event.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct SecurityEvent {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub user_id: Option<UserId>,
    pub anonymous_user_id: String,
    pub source: String,
    pub argument: serde_json::Value,
    pub version: String,
    pub timestamp: DateTime<Utc>,
}

/// Writes and reads `security_event_logs`.
#[derive(Debug, Clone)]
pub struct SecurityEventStore {
    pool: PgPool,
}

impl SecurityEventStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records an event and returns its id. A `null` argument is stored as `{}`.
    pub async fn insert(&self, event: &NewSecurityEvent) -> Result<i64> {
        let argument = if event.argument.is_null() {
            serde_json::Value::Object(Default::default())
        } else {
            event.argument.clone()
        };
        Ok(sqlx::query_scalar(
            "INSERT INTO security_event_logs
                 (name, url, user_id, anonymous_user_id, source, argument, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(&event.name)
        .bind(&event.url)
        .bind(event.user_id)
        .bind(&event.anonymous_user_id)
        .bind(&event.source)
        .bind(argument)
        .bind(&event.version)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The newest events, optionally only those of one user, newest first.
    pub async fn list_recent(
        &self,
        user: Option<UserId>,
        limit: i64,
    ) -> Result<Vec<SecurityEvent>> {
        Ok(sqlx::query_as(
            "SELECT id, name, url, user_id, anonymous_user_id, source, argument, version, timestamp
             FROM security_event_logs
             WHERE ($1::bigint IS NULL OR user_id = $1)
             ORDER BY timestamp DESC, id DESC LIMIT $2",
        )
        .bind(user)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}
