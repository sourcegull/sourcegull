//! `site_config` store: versioned site configuration.

use crate::notify::{channels, notify};
use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_types::{SiteConfigId, UserId};
use sqlx::PgPool;

/// One version of the site configuration.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SiteConfig {
    pub id: SiteConfigId,
    /// JSONC text, verbatim.
    pub contents: String,
    pub author_user_id: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// Reads and writes `site_config`.
#[derive(Debug, Clone)]
pub struct SiteConfigStore {
    pool: PgPool,
}

impl SiteConfigStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The newest version, if any was ever written.
    pub async fn latest(&self) -> Result<Option<SiteConfig>> {
        Ok(sqlx::query_as(
            "SELECT id, contents, author_user_id, created_at FROM site_config ORDER BY id DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Writes a new version, provided the latest version is still
    /// `expected_last_id` (`None` = no version exists yet). Otherwise fails
    /// with [`DbError::Conflict`]. Sends a `site_config` notification with the
    /// new id on commit.
    pub async fn create(
        &self,
        contents: &str,
        author: Option<UserId>,
        expected_last_id: Option<SiteConfigId>,
    ) -> Result<SiteConfig> {
        let mut tx = self.pool.begin().await?;
        // Serializes writers; readers are not blocked.
        sqlx::query("LOCK TABLE site_config IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        let actual: Option<SiteConfigId> = sqlx::query_scalar("SELECT max(id) FROM site_config")
            .fetch_one(&mut *tx)
            .await?;
        if actual != expected_last_id {
            return Err(DbError::Conflict {
                expected: expected_last_id.map(SiteConfigId::get),
                actual: actual.map(SiteConfigId::get),
            });
        }
        let row: SiteConfig = sqlx::query_as(
            "INSERT INTO site_config (contents, author_user_id) VALUES ($1, $2)
             RETURNING id, contents, author_user_id, created_at",
        )
        .bind(contents)
        .bind(author)
        .fetch_one(&mut *tx)
        .await?;
        notify(&mut *tx, channels::SITE_CONFIG, &row.id.to_string()).await?;
        tx.commit().await?;
        Ok(row)
    }
}
