//! `external_services` store: code host connections. Configurations are encrypted
//! with the `external_service_config` data key and only decrypted on demand.

use std::sync::Arc;

use crate::stores::sync_jobs::{SyncReason, enqueue_sync_job};
use crate::{DbError, Result};
use chrono::{DateTime, Utc};
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_types::{ExternalService, ExternalServiceId, ExternalServiceKind};
use sqlx::{PgExecutor, PgPool};

const COLUMNS: &str = "id, kind, display_name, webhook_uuid, last_sync_at, next_sync_at, \
     last_full_sync_at, sync_watermark, last_sync_error, created_at, updated_at, deleted_at";

/// Fields for [`ExternalServiceStore::create`].
#[derive(Debug, Clone)]
pub struct NewExternalService {
    pub kind: ExternalServiceKind,
    pub display_name: String,
    /// The JSONC configuration, already validated by the caller (plaintext; encrypted here).
    pub config: String,
}

/// Changes for [`ExternalServiceStore::update`]; `None` keeps the current value.
#[derive(Debug, Clone, Default)]
pub struct ExternalServiceUpdate {
    pub display_name: Option<String>,
    /// The new JSONC configuration (plaintext). Changing it resets the incremental watermark,
    /// so the next sync is a full listing, and queues that sync.
    pub config: Option<String>,
    /// The configuration change only touches settings discovery does not use (webhook
    /// secrets): keep the sync state and queue no sync.
    pub config_keeps_sync: bool,
}

/// The outcome of one discovery sync, recorded by [`ExternalServiceStore::record_sync`].
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Whether this was a full listing.
    pub full: bool,
    /// The new incremental watermark (`None` keeps the current one).
    pub watermark: Option<DateTime<Utc>>,
    /// The error, if the sync failed.
    pub error: Option<String>,
    /// When the next scheduled sync is due.
    pub next_sync_at: DateTime<Utc>,
}

/// Reads and writes `external_services`. Deleted connections are invisible to lookups.
///
/// Reading or writing a configuration needs the keyring; without one (no key file configured)
/// those calls fail with [`DbError::Invalid`] while the other calls work.
#[derive(Debug, Clone)]
pub struct ExternalServiceStore {
    pool: PgPool,
    keyring: Option<Arc<Keyring>>,
}

impl ExternalServiceStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool, keyring: Option<Arc<Keyring>>) -> Self {
        Self { pool, keyring }
    }

    fn keyring(&self) -> Result<&Keyring> {
        self.keyring.as_deref().ok_or(DbError::Invalid(
            "encryption keys are not configured (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS)",
        ))
    }

    fn encrypt(&self, config: &str) -> Result<Vec<u8>> {
        Ok(self
            .keyring()?
            .encrypt(RecordClass::ExternalServiceConfig, config.as_bytes())?)
    }

    fn decrypt(&self, blob: &[u8]) -> Result<String> {
        let plain = self
            .keyring()?
            .decrypt(RecordClass::ExternalServiceConfig, blob)?;
        String::from_utf8(plain.to_vec()).map_err(|_| DbError::Invalid("config is not UTF-8"))
    }

    /// Creates a connection and queues its first sync.
    pub async fn create(&self, new: &NewExternalService) -> Result<ExternalService> {
        let enc = self.encrypt(&new.config)?;
        let mut tx = self.pool.begin().await?;
        let svc: ExternalService = sqlx::query_as(&format!(
            "INSERT INTO external_services (kind, display_name, config_enc)
             VALUES ($1, $2, $3) RETURNING {COLUMNS}"
        ))
        .bind(new.kind)
        .bind(&new.display_name)
        .bind(enc)
        .fetch_one(&mut *tx)
        .await?;
        enqueue_sync_job(&mut tx, svc.id, SyncReason::ConfigChanged).await?;
        tx.commit().await?;
        Ok(svc)
    }

    /// Looks up a live connection by id.
    pub async fn get(&self, id: ExternalServiceId) -> Result<Option<ExternalService>> {
        get(&self.pool, id).await
    }

    /// Looks up a live connection by its webhook UUID.
    pub async fn get_by_webhook_uuid(&self, uuid: &str) -> Result<Option<ExternalService>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM external_services WHERE webhook_uuid = $1 AND deleted_at IS NULL"
        ))
        .bind(uuid)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Lists live connections ordered by id, optionally of one kind.
    pub async fn list(
        &self,
        kind: Option<ExternalServiceKind>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ExternalService>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM external_services
             WHERE deleted_at IS NULL AND ($1::text IS NULL OR kind = $1)
             ORDER BY id LIMIT $2 OFFSET $3"
        ))
        .bind(kind.map(ExternalServiceKind::as_str))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Number of live connections, optionally of one kind.
    pub async fn count(&self, kind: Option<ExternalServiceKind>) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM external_services
             WHERE deleted_at IS NULL AND ($1::text IS NULL OR kind = $1)",
        )
        .bind(kind.map(ExternalServiceKind::as_str))
        .fetch_one(&self.pool)
        .await?)
    }

    /// The decrypted configuration of a live connection.
    pub async fn config(&self, id: ExternalServiceId) -> Result<String> {
        let blob: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT config_enc FROM external_services WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        self.decrypt(&blob.ok_or(DbError::NotFound("external service"))?)
    }

    /// Every live connection of `kind` with its decrypted configuration (for webhook routing
    /// by secret).
    pub async fn list_with_config(
        &self,
        kind: ExternalServiceKind,
    ) -> Result<Vec<(ExternalService, String)>> {
        let rows: Vec<WithConfig> = sqlx::query_as(&format!(
            "SELECT {COLUMNS}, config_enc FROM external_services
             WHERE deleted_at IS NULL AND kind = $1 ORDER BY id"
        ))
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| Ok((r.svc, self.decrypt(&r.config_enc)?)))
            .collect()
    }

    /// Applies `update`. A configuration change forces the next sync to be a full listing and
    /// queues it, unless [`ExternalServiceUpdate::config_keeps_sync`] is set.
    pub async fn update(
        &self,
        id: ExternalServiceId,
        update: &ExternalServiceUpdate,
    ) -> Result<ExternalService> {
        let mut tx = self.pool.begin().await?;
        let svc = self.apply_update(&mut tx, id, update).await?;
        tx.commit().await?;
        Ok(svc)
    }

    async fn apply_update(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
        id: ExternalServiceId,
        update: &ExternalServiceUpdate,
    ) -> Result<ExternalService> {
        let enc = update
            .config
            .as_deref()
            .map(|c| self.encrypt(c))
            .transpose()?;
        let svc: Option<ExternalService> = sqlx::query_as(&format!(
            "UPDATE external_services SET
                 display_name = coalesce($2, display_name),
                 config_enc = coalesce($3, config_enc),
                 sync_watermark = CASE WHEN $3 IS NULL OR $4 THEN sync_watermark END,
                 last_full_sync_at = CASE WHEN $3 IS NULL OR $4 THEN last_full_sync_at END,
                 updated_at = now()
             WHERE id = $1 AND deleted_at IS NULL
             RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(&update.display_name)
        .bind(&enc)
        .bind(update.config_keeps_sync)
        .fetch_optional(&mut **tx)
        .await?;
        let svc = svc.ok_or(DbError::NotFound("external service"))?;
        if enc.is_some() && !update.config_keeps_sync {
            enqueue_sync_job(tx, id, SyncReason::ConfigChanged).await?;
        }
        Ok(svc)
    }

    /// Locks a live connection's row and returns its decrypted configuration, so the caller
    /// can replace part of it ([`LockedConfig::replace`]) without losing a concurrent update.
    /// Other updates of the connection wait until the lock is released.
    pub async fn lock_config(&self, id: ExternalServiceId) -> Result<LockedConfig> {
        let mut tx = self.pool.begin().await?;
        let blob: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT config_enc FROM external_services
             WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let config = self.decrypt(&blob.ok_or(DbError::NotFound("external service"))?)?;
        Ok(LockedConfig {
            store: self.clone(),
            tx,
            id,
            config,
        })
    }

    /// Records whether the connection's private repos are readable by every user (its config
    /// has no `authorization` block). Notifies `authz` when the flag changes.
    pub async fn set_unrestricted(&self, id: ExternalServiceId, unrestricted: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE external_services SET unrestricted = $2
             WHERE id = $1 AND unrestricted <> $2",
        )
        .bind(id)
        .bind(unrestricted)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() > 0 {
            crate::notify::notify(
                &mut *tx,
                crate::notify::channels::AUTHZ,
                crate::stores::authz_payload::UNRESTRICTED,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Soft-deletes a connection: it stops syncing, its repo links are removed and repos no
    /// other connection lists are soft-deleted. Returns the number of repos deleted.
    pub async fn soft_delete(&self, id: ExternalServiceId) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE external_services SET deleted_at = now(), next_sync_at = NULL, updated_at = now()
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("external service"));
        }
        sqlx::query(
            "UPDATE external_service_sync_jobs SET cancel = true,
                 state = CASE WHEN state = 'queued' THEN 'canceled' ELSE state END,
                 finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
             WHERE external_service_id = $1 AND state IN ('queued', 'processing')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let deleted = crate::stores::repos::unlink_repos(&mut tx, id, &[]).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Records the result of a sync: `last_sync_at`, the error (cleared on success), the next
    /// due time and, on success, the watermark and the full-listing time.
    pub async fn record_sync(&self, id: ExternalServiceId, outcome: &SyncOutcome) -> Result<()> {
        sqlx::query(
            "UPDATE external_services SET
                 last_sync_at = now(),
                 last_sync_error = $2,
                 next_sync_at = CASE WHEN deleted_at IS NULL THEN $3 END,
                 sync_watermark = CASE WHEN $2 IS NULL THEN coalesce($4, sync_watermark)
                                       ELSE sync_watermark END,
                 last_full_sync_at = CASE WHEN $2 IS NULL AND $5 THEN now()
                                          ELSE last_full_sync_at END
             WHERE id = $1",
        )
        .bind(id)
        .bind(&outcome.error)
        .bind(outcome.next_sync_at)
        .bind(outcome.watermark)
        .bind(outcome.full)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Number of live repos the connection lists.
    pub async fn repo_count(&self, id: ExternalServiceId) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM external_service_repos esr
             JOIN repo r ON r.id = esr.repo_id AND r.deleted_at IS NULL
             WHERE esr.external_service_id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }
}

/// A connection's configuration with its row locked ([`ExternalServiceStore::lock_config`]).
/// Dropping it releases the lock without changes.
pub struct LockedConfig {
    store: ExternalServiceStore,
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    id: ExternalServiceId,
    /// The decrypted configuration.
    pub config: String,
}

impl std::fmt::Debug for LockedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockedConfig")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl LockedConfig {
    /// Stores `config` (credentials only: the sync state is kept and no sync is queued) and
    /// releases the lock.
    pub async fn replace(mut self, config: &str) -> Result<()> {
        let enc = self.store.encrypt(config)?;
        sqlx::query(
            "UPDATE external_services SET config_enc = $2, updated_at = now() WHERE id = $1",
        )
        .bind(self.id)
        .bind(enc)
        .execute(&mut *self.tx)
        .await?;
        self.tx.commit().await?;
        Ok(())
    }

    /// Applies `update` as [`ExternalServiceStore::update`] does and releases the lock.
    pub async fn update(mut self, update: &ExternalServiceUpdate) -> Result<ExternalService> {
        let svc = self
            .store
            .apply_update(&mut self.tx, self.id, update)
            .await?;
        self.tx.commit().await?;
        Ok(svc)
    }
}

#[derive(sqlx::FromRow)]
struct WithConfig {
    #[sqlx(flatten)]
    svc: ExternalService,
    config_enc: Vec<u8>,
}

async fn get<'e, E: PgExecutor<'e>>(
    executor: E,
    id: ExternalServiceId,
) -> Result<Option<ExternalService>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM external_services WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(executor)
    .await?)
}
