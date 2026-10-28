//! Configuration as code: the declarative files a deployment uses (`declared_sources`), the
//! objects the resources file owns (`declared_resources`), its site admins
//! (`declared_site_admins`) and what it granted users (`declared_grants`).
//!
//! The leader worker writes these tables when it applies the files; the API reads them to keep
//! owned objects read-only ([`DeclaredStore::ownership`]).

use chrono::{DateTime, Utc};
use sourcegull_types::UserId;
use sqlx::PgPool;

use crate::Result;

/// `declared_sources.source` of the resources file.
pub const RESOURCES_SOURCE: &str = "resources";
/// `declared_sources.source` of the global settings file.
pub const GLOBAL_SETTINGS_SOURCE: &str = "global_settings";
/// `declared_grants.kind` of site admin grants.
pub const SITE_ADMIN_GRANT: &str = "siteAdmins";
/// `declared_grants.kind` of role grants.
pub const ROLE_GRANT: &str = "roles";

/// A configured declarative file and its last application.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeclaredSource {
    pub source: String,
    /// The file's path in the worker that applies it.
    pub path: String,
    /// The sections the resources file manages.
    pub kinds: Vec<String>,
    pub exclusive: bool,
    /// Hash of the last applied resolved document.
    pub applied_sha256: Option<Vec<u8>>,
    pub applied_at: Option<DateTime<Utc>>,
    /// Why the current file could not be applied; the last applied state stays.
    pub error: Option<String>,
    pub error_at: Option<DateTime<Utc>>,
    pub checked_at: DateTime<Utc>,
}

/// An object owned by a declared entry.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeclaredObject {
    pub kind: String,
    pub key: String,
    pub object_id: String,
    pub applied_sha256: Vec<u8>,
}

/// Why an object cannot be changed through the UI or API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// The resources file declares it under this key.
    Declared { key: String },
    /// The resources file owns its whole kind (`exclusive`).
    Exclusive,
}

/// Reads and writes the configuration-as-code tables.
#[derive(Debug, Clone)]
pub struct DeclaredStore {
    pool: PgPool,
}

impl DeclaredStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every configured file.
    pub async fn sources(&self) -> Result<Vec<DeclaredSource>> {
        Ok(sqlx::query_as(
            "SELECT source, path, kinds, exclusive, applied_sha256, applied_at, error, error_at,
                    checked_at
             FROM declared_sources ORDER BY source",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// One configured file.
    pub async fn source(&self, source: &str) -> Result<Option<DeclaredSource>> {
        Ok(sqlx::query_as(
            "SELECT source, path, kinds, exclusive, applied_sha256, applied_at, error, error_at,
                    checked_at
             FROM declared_sources WHERE source = $1",
        )
        .bind(source)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records a successful application of `source`, clearing any error.
    pub async fn record_applied(
        &self,
        source: &str,
        path: &str,
        kinds: &[String],
        exclusive: bool,
        sha256: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO declared_sources
                 (source, path, kinds, exclusive, applied_sha256, applied_at, checked_at)
             VALUES ($1, $2, $3, $4, $5, now(), now())
             ON CONFLICT (source) DO UPDATE SET
                 path = EXCLUDED.path, kinds = EXCLUDED.kinds, exclusive = EXCLUDED.exclusive,
                 applied_sha256 = EXCLUDED.applied_sha256,
                 applied_at = CASE WHEN declared_sources.applied_sha256
                                        IS NOT DISTINCT FROM EXCLUDED.applied_sha256
                                   THEN declared_sources.applied_at ELSE now() END,
                 error = NULL, error_at = NULL, checked_at = now()",
        )
        .bind(source)
        .bind(path)
        .bind(kinds)
        .bind(exclusive)
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records why `source` could not be applied, keeping what it manages.
    pub async fn record_error(&self, source: &str, path: &str, error: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO declared_sources (source, path, error, error_at, checked_at)
             VALUES ($1, $2, $3, now(), now())
             ON CONFLICT (source) DO UPDATE SET
                 path = EXCLUDED.path, error = EXCLUDED.error,
                 error_at = CASE WHEN declared_sources.error IS NOT DISTINCT FROM EXCLUDED.error
                                 THEN declared_sources.error_at ELSE now() END,
                 checked_at = now()",
        )
        .bind(source)
        .bind(path)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forgets `source` (its file is no longer configured). Returns whether it was recorded.
    pub async fn remove_source(&self, source: &str) -> Result<bool> {
        Ok(
            sqlx::query("DELETE FROM declared_sources WHERE source = $1")
                .bind(source)
                .execute(&self.pool)
                .await?
                .rows_affected()
                > 0,
        )
    }

    /// The objects owned by entries of `kind`.
    pub async fn objects(&self, kind: &str) -> Result<Vec<DeclaredObject>> {
        Ok(sqlx::query_as(
            "SELECT kind, key, object_id, applied_sha256 FROM declared_resources
             WHERE kind = $1 ORDER BY key",
        )
        .bind(kind)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every owned object.
    pub async fn all_objects(&self) -> Result<Vec<DeclaredObject>> {
        Ok(sqlx::query_as(
            "SELECT kind, key, object_id, applied_sha256 FROM declared_resources
             ORDER BY kind, key",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records that the entry `key` of `kind` owns `object_id` with contents `sha256`.
    pub async fn set_object(
        &self,
        kind: &str,
        key: &str,
        object_id: &str,
        sha256: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO declared_resources (kind, key, object_id, applied_sha256)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (kind, key) DO UPDATE SET
                 object_id = EXCLUDED.object_id, applied_sha256 = EXCLUDED.applied_sha256,
                 updated_at = now()",
        )
        .bind(kind)
        .bind(key)
        .bind(object_id)
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forgets the entry `key` of `kind` (its object is deleted or released).
    pub async fn remove_object(&self, kind: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM declared_resources WHERE kind = $1 AND key = $2")
            .bind(kind)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Releases every object of `kind` to the UI (its section was removed). Returns how many.
    pub async fn release_kind(&self, kind: &str) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM declared_resources WHERE kind = $1")
                .bind(kind)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// The kinds that own objects.
    pub async fn kinds_with_objects(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT DISTINCT kind FROM declared_resources ORDER BY kind")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Whether the object `object_id` of `kind` may not be changed through the UI or API, and
    /// why.
    pub async fn ownership(&self, kind: &str, object_id: &str) -> Result<Option<Ownership>> {
        let (key, exclusive): (Option<String>, bool) = sqlx::query_as(
            "SELECT (SELECT key FROM declared_resources WHERE kind = $1 AND object_id = $2),
                    EXISTS (SELECT 1 FROM declared_sources
                            WHERE source = 'resources' AND exclusive AND $1 = ANY (kinds))",
        )
        .bind(kind)
        .bind(object_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(match (key, exclusive) {
            (Some(key), _) => Some(Ownership::Declared { key }),
            (None, true) => Some(Ownership::Exclusive),
            (None, false) => None,
        })
    }

    /// The key owning each of `object_ids` of `kind` (for lists).
    pub async fn owners(&self, kind: &str, object_ids: &[String]) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(
            "SELECT object_id, key FROM declared_resources
             WHERE kind = $1 AND object_id = ANY ($2)",
        )
        .bind(kind)
        .bind(object_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Whether the resources file owns every object of `kind`, so none can be created in the UI.
    pub async fn is_exclusive(&self, kind: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM declared_sources
                            WHERE source = 'resources' AND exclusive AND $1 = ANY (kinds))",
        )
        .bind(kind)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Replaces the declared site admin emails (`None`: the section is absent).
    pub async fn set_site_admins(&self, emails: Option<&[String]>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM declared_site_admins")
            .execute(&mut *tx)
            .await?;
        if let Some(emails) = emails {
            let lower: Vec<String> = emails.iter().map(|e| e.to_lowercase()).collect();
            sqlx::query(
                "INSERT INTO declared_site_admins (email)
                 SELECT DISTINCT e FROM unnest($1::text[]) AS e",
            )
            .bind(&lower)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The declared site admin emails.
    pub async fn site_admin_emails(&self) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT email FROM declared_site_admins ORDER BY email")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Live users with one of `emails` (compared trimmed and lower-cased) verified.
    pub async fn users_with_verified_emails(&self, emails: &[String]) -> Result<Vec<UserId>> {
        let lower: Vec<String> = emails.iter().map(|e| e.trim().to_lowercase()).collect();
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT u.id FROM users u JOIN user_emails e ON e.user_id = u.id
             WHERE u.deleted_at IS NULL AND e.verified_at IS NOT NULL
               AND lower(e.email) = ANY ($1)
             ORDER BY u.id",
        )
        .bind(&lower)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Live site admins.
    pub async fn site_admins(&self) -> Result<Vec<UserId>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM users WHERE site_admin AND deleted_at IS NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The users the file granted `kind`/`key`.
    pub async fn grants(&self, kind: &str, key: &str) -> Result<Vec<UserId>> {
        Ok(sqlx::query_scalar(
            "SELECT user_id FROM declared_grants WHERE kind = $1 AND key = $2 ORDER BY user_id",
        )
        .bind(kind)
        .bind(key)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records that the file granted `kind`/`key` to `user`.
    pub async fn add_grant(&self, kind: &str, key: &str, user: UserId) -> Result<()> {
        sqlx::query(
            "INSERT INTO declared_grants (kind, key, user_id) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(kind)
        .bind(key)
        .bind(user)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forgets a grant.
    pub async fn remove_grant(&self, kind: &str, key: &str, user: UserId) -> Result<()> {
        sqlx::query("DELETE FROM declared_grants WHERE kind = $1 AND key = $2 AND user_id = $3")
            .bind(kind)
            .bind(key)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forgets every grant of `kind` (its section was removed).
    pub async fn remove_grants_of_kind(&self, kind: &str) -> Result<()> {
        sqlx::query("DELETE FROM declared_grants WHERE kind = $1")
            .bind(kind)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Forgets every grant of `kind`/`key` (the role was deleted).
    pub async fn remove_grants(&self, kind: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM declared_grants WHERE kind = $1 AND key = $2")
            .bind(kind)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
