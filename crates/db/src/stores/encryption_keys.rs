//! `encryption_keys` store: persisted, KEK-wrapped data keys.

use std::sync::Arc;

use crate::notify::{channels, notify};
use crate::{DbError, Result};
use sourcegull_crypto::{KeyProvider, KeyState, Keyring, RecordClass, RotateScope, WrappedKey};
use sqlx::PgPool;

/// Advisory lock key serializing data key creation across processes (a fixed constant).
const DEK_LOCK_KEY: i64 = 0x7365_656b_6572_646b;

/// Loads and creates data keys (`encryption_keys`, one row per record class and version;
/// `key_name` is the [`RecordClass`] storage name).
#[derive(Debug, Clone)]
pub struct EncryptionKeyStore {
    pool: PgPool,
}

impl EncryptionKeyStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every usable (active or retired) data key, wrapped.
    pub async fn list(&self) -> Result<Vec<WrappedKey>> {
        let rows = sqlx::query_as(SELECT_KEYS).fetch_all(&self.pool).await?;
        parse_rows(rows)
    }

    /// Loads the keyring, creating (and persisting) a data key for every record class that has
    /// no active one yet. Safe to call from several processes at once: creation is serialized
    /// with a transaction-scoped advisory lock, so every process ends up with the same keys.
    ///
    /// When the provider's current KEK (the key file's last line) is not the one that wrapped
    /// the active data keys, this is a key rotation ([`RotateScope::KekChanged`]): every
    /// data key is re-wrapped with the new KEK, and each class gets a new data key while the
    /// old one is retired (decrypt-only). The re-encryption out-of-band migration then moves
    /// records to the new keys.
    pub async fn load_keyring(&self, provider: Arc<dyn KeyProvider>) -> Result<Keyring> {
        self.load_and_rotate(provider, RotateScope::KekChanged)
            .await
            .map(|(ring, _)| ring)
    }

    /// Loads the keyring and gives every record class a new data key even if the KEK did not
    /// change (`sourcegull keys rotate`). Returns the keyring and how many key rows changed.
    pub async fn rotate(&self, provider: Arc<dyn KeyProvider>) -> Result<(Keyring, usize)> {
        self.load_and_rotate(provider, RotateScope::All).await
    }

    /// [`EncryptionKeyStore::load_keyring`] with a rotation scope; also returns how many key
    /// rows were created or changed (excluding the first start's initial keys).
    pub async fn load_and_rotate(
        &self,
        provider: Arc<dyn KeyProvider>,
        scope: RotateScope,
    ) -> Result<(Keyring, usize)> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(DEK_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as(SELECT_KEYS).fetch_all(&mut *tx).await?;
        let wrapped = parse_rows(rows)?;
        let fresh = wrapped.is_empty();
        let mut keyring = Keyring::from_wrapped(provider, wrapped).map_err(DbError::Crypto)?;
        let mut changed = keyring.ensure_deks().map_err(DbError::Crypto)?;
        if !fresh {
            changed.extend(keyring.rotate(scope).map_err(DbError::Crypto)?);
        }
        // Retired keys are written before the new active ones (one active key per class).
        changed.sort_by_key(|k| (k.class, k.version));
        for key in &changed {
            sqlx::query(
                "INSERT INTO encryption_keys (key_name, version, wrapped_key, state)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (key_name, version) DO UPDATE SET
                     wrapped_key = EXCLUDED.wrapped_key, state = EXCLUDED.state,
                     retired_at = CASE WHEN EXCLUDED.state = 'retired'
                         THEN COALESCE(encryption_keys.retired_at, now()) END",
            )
            .bind(key.class.as_str())
            .bind(key.version)
            .bind(&key.wrapped)
            .bind(key.state.as_str())
            .execute(&mut *tx)
            .await?;
        }
        if !changed.is_empty() && !fresh {
            notify(&mut *tx, channels::ENCRYPTION_KEYS, "rotated").await?;
        }
        tx.commit().await?;
        Ok((keyring, if fresh { 0 } else { changed.len() }))
    }

    /// Reloads a shared keyring's data keys from the table (after a rotation elsewhere).
    pub async fn reload(
        &self,
        keyring: &Keyring,
        provider: Option<Arc<dyn KeyProvider>>,
    ) -> Result<()> {
        let keys = self.list().await?;
        keyring.reload(provider, keys).map_err(DbError::Crypto)
    }

    /// The active data key version of every record class (`key_name` → version).
    pub async fn active_versions(&self) -> Result<Vec<(String, i32)>> {
        Ok(sqlx::query_as(
            "SELECT key_name, version FROM encryption_keys WHERE state = 'active' ORDER BY key_name",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

const SELECT_KEYS: &str = "SELECT key_name, version, wrapped_key, state FROM encryption_keys
     WHERE state <> 'destroyed' ORDER BY key_name, version";

fn parse_rows(rows: Vec<(String, i32, Vec<u8>, String)>) -> Result<Vec<WrappedKey>> {
    rows.into_iter()
        .map(|(name, version, wrapped, state)| {
            Ok(WrappedKey {
                class: name.parse::<RecordClass>()?,
                version,
                wrapped,
                state: state.parse::<KeyState>()?,
            })
        })
        .collect::<Result<_, sourcegull_crypto::CryptoError>>()
        .map_err(DbError::Crypto)
}
