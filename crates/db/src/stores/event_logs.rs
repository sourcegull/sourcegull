//! `event_logs` store: local usage events. Nothing leaves the instance; the analytics
//! rollups (`sourcegull-analytics`) read the table and the worker prunes it after
//! `eventLogs.retention`.

use crate::Result;
use sourcegull_types::UserId;
use sqlx::PgPool;

/// A usage event to record.
#[derive(Debug, Clone, Default)]
pub struct NewUsageEvent {
    /// Event name, e.g. `SearchResultsQueried`, `ViewBlob`.
    pub name: String,
    pub url: String,
    pub user_id: Option<UserId>,
    /// The anonymous visitor id (signed-out users), `""` when unknown or for signed-in users.
    pub anonymous_user_id: String,
    /// Where it came from: `WEB`, `BACKEND`, `CODEHOSTINTEGRATION`, `IDEEXTENSION`, ….
    pub source: String,
    /// Event-specific (public) details; `null` is stored as `{}`.
    pub argument: serde_json::Value,
}

/// Writes `event_logs`.
#[derive(Debug, Clone)]
pub struct EventLogStore {
    pool: PgPool,
}

impl EventLogStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records events in one statement.
    pub async fn insert(&self, events: &[NewUsageEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut names = Vec::with_capacity(events.len());
        let mut urls = Vec::with_capacity(events.len());
        let mut users = Vec::with_capacity(events.len());
        let mut anons = Vec::with_capacity(events.len());
        let mut sources = Vec::with_capacity(events.len());
        let mut args = Vec::with_capacity(events.len());
        for e in events {
            names.push(e.name.as_str());
            urls.push(e.url.as_str());
            users.push(e.user_id.map(|u| u.get()));
            // Signed-in users are counted by id only.
            anons.push(if e.user_id.is_some() {
                ""
            } else {
                e.anonymous_user_id.as_str()
            });
            sources.push(e.source.as_str());
            args.push(if e.argument.is_null() {
                serde_json::Value::Object(Default::default())
            } else {
                e.argument.clone()
            });
        }
        sqlx::query(
            "INSERT INTO event_logs (name, url, user_id, anonymous_user_id, source, argument, version)
             SELECT n, u, uid, a, s, arg, $7
             FROM unnest($1::text[], $2::text[], $3::bigint[], $4::text[], $5::text[], $6::jsonb[])
                 AS t(n, u, uid, a, s, arg)",
        )
        .bind(&names)
        .bind(&urls)
        .bind(&users)
        .bind(&anons)
        .bind(&sources)
        .bind(&args)
        .bind(env!("CARGO_PKG_VERSION"))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
