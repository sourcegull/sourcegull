//! `external_service_token_checks` store: when each code host connection's token expires, as
//! last checked, and which expiry alert has been sent for it.

use crate::Result;
use chrono::{DateTime, Utc};
use sourcegull_types::ExternalServiceId;
use sqlx::PgPool;

const COLUMNS: &str =
    "external_service_id, checked_at, config_updated_at, expiry, expires_at, error, alerted_days";

/// A token's expiry as stored (`expiry`, `expires_at`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredExpiry {
    /// The code host does not say.
    Unknown,
    /// No expiry date.
    Never,
    /// Expires at this time.
    At(DateTime<Utc>),
}

impl StoredExpiry {
    fn columns(self) -> (&'static str, Option<DateTime<Utc>>) {
        match self {
            Self::Unknown => ("unknown", None),
            Self::Never => ("never", None),
            Self::At(at) => ("at", Some(at)),
        }
    }
}

/// One connection's last token check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCheck {
    pub external_service_id: ExternalServiceId,
    pub checked_at: DateTime<Utc>,
    /// The connection's `updated_at` when it was checked.
    pub config_updated_at: DateTime<Utc>,
    /// The expiry of the last successful check.
    pub expiry: StoredExpiry,
    /// Why the last check failed.
    pub error: Option<String>,
    /// The smallest number of days before the expiry an alert was sent for (0: expired).
    pub alerted_days: Option<i32>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for TokenCheck {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let expiry: String = row.try_get("expiry")?;
        let expires_at: Option<DateTime<Utc>> = row.try_get("expires_at")?;
        Ok(Self {
            external_service_id: row.try_get("external_service_id")?,
            checked_at: row.try_get("checked_at")?,
            config_updated_at: row.try_get("config_updated_at")?,
            expiry: match (expiry.as_str(), expires_at) {
                ("at", Some(at)) => StoredExpiry::At(at),
                ("never", _) => StoredExpiry::Never,
                _ => StoredExpiry::Unknown,
            },
            error: row.try_get("error")?,
            alerted_days: row.try_get("alerted_days")?,
        })
    }
}

/// Reads and writes `external_service_token_checks`.
#[derive(Debug, Clone)]
pub struct TokenCheckStore {
    pool: PgPool,
}

impl TokenCheckStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Live connections whose token is due for a check: never checked, configuration changed
    /// since the check, or checked before `stale_before`. Oldest check first.
    pub async fn due(&self, stale_before: DateTime<Utc>) -> Result<Vec<ExternalServiceId>> {
        Ok(sqlx::query_scalar(
            "SELECT s.id FROM external_services s
             LEFT JOIN external_service_token_checks c ON c.external_service_id = s.id
             WHERE s.deleted_at IS NULL
               AND (c.external_service_id IS NULL OR c.config_updated_at <> s.updated_at
                    OR c.checked_at < $1)
             ORDER BY c.checked_at NULLS FIRST, s.id",
        )
        .bind(stale_before)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records a successful check of the configuration last updated at `config_updated_at`.
    /// A changed expiry clears `alerted_days`, so the new expiry gets its own alerts.
    pub async fn record(
        &self,
        id: ExternalServiceId,
        config_updated_at: DateTime<Utc>,
        expiry: StoredExpiry,
    ) -> Result<TokenCheck> {
        let (kind, at) = expiry.columns();
        Ok(sqlx::query_as(&format!(
            "INSERT INTO external_service_token_checks AS c
                 (external_service_id, checked_at, config_updated_at, expiry, expires_at)
             VALUES ($1, now(), $2, $3, $4)
             ON CONFLICT (external_service_id) DO UPDATE SET
                 checked_at = now(), config_updated_at = $2, expiry = $3, expires_at = $4,
                 error = NULL,
                 alerted_days = CASE
                     WHEN c.expiry = $3 AND c.expires_at IS NOT DISTINCT FROM $4
                     THEN c.alerted_days END
             RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(config_updated_at)
        .bind(kind)
        .bind(at)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records a failed check, keeping the expiry of the last successful one.
    pub async fn record_error(
        &self,
        id: ExternalServiceId,
        config_updated_at: DateTime<Utc>,
        error: &str,
    ) -> Result<TokenCheck> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO external_service_token_checks
                 (external_service_id, checked_at, config_updated_at, expiry, error)
             VALUES ($1, now(), $2, 'unknown', $3)
             ON CONFLICT (external_service_id) DO UPDATE SET
                 checked_at = now(), config_updated_at = $2, error = $3
             RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(config_updated_at)
        .bind(error)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records that the alert for `days` before the expiry was sent.
    pub async fn set_alerted(&self, id: ExternalServiceId, days: i32) -> Result<()> {
        sqlx::query(
            "UPDATE external_service_token_checks SET alerted_days = $2
             WHERE external_service_id = $1",
        )
        .bind(id)
        .bind(days)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The last check of one connection.
    pub async fn get(&self, id: ExternalServiceId) -> Result<Option<TokenCheck>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM external_service_token_checks WHERE external_service_id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The checks of live connections whose token has an expiry date, soonest first.
    pub async fn expiring(&self) -> Result<Vec<TokenCheck>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {} FROM external_service_token_checks c
             JOIN external_services s ON s.id = c.external_service_id
             WHERE s.deleted_at IS NULL AND c.expiry = 'at'
             ORDER BY c.expires_at, c.external_service_id",
            COLUMNS
                .split(", ")
                .map(|c| format!("c.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .fetch_all(&self.pool)
        .await?)
    }
}
