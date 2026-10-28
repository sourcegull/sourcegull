//! Re-encryption of secrets after a key rotation, out-of-band migration 1.
//!
//! Every ciphertext names the data key version it was encrypted with (bytes 1..5 of the
//! header, see `sourcegull_crypto::Keyring`). A rotation retires each class's data key and creates
//! a new active one; this migration finds values whose version is not the class's active one
//! and rewrites them in batches, so the retired keys end up protecting nothing.
//!
//! - **Progress** is measured in SQL from the headers (no keys needed): the share of
//!   encrypted values already on their class's active key. New rotations make it drop below
//!   1 again, and the runner picks the work up.
//! - **Up** reloads the shared keyring from `encryption_keys` (another process may have
//!   rotated), then re-encrypts up to [`BATCH_SIZE`] values. Each update is a compare-and-swap
//!   on the old ciphertext, so a concurrent write wins.
//! - **Down** is a no-op ([`Migrator::reversible`] is false): retired keys stay in the keyring
//!   and keep decrypting, so values on the new keys are readable by every version that can
//!   load the keyring, and there is nothing to undo.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use async_trait::async_trait;
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_db::{Db, Fence};

use crate::Migrator;

/// Values re-encrypted per batch.
pub const BATCH_SIZE: i64 = 500;

/// An encrypted column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptedColumn {
    /// Table (with a `bigint` `id` primary key).
    pub table: &'static str,
    /// The `bytea` column with the ciphertext.
    pub column: &'static str,
    /// Its record class (data key).
    pub class: RecordClass,
    /// A column that mirrors the data key version, updated along with the value.
    pub version_column: Option<&'static str>,
}

const fn col(table: &'static str, column: &'static str, class: RecordClass) -> EncryptedColumn {
    EncryptedColumn {
        table,
        column,
        class,
        version_column: None,
    }
}

/// Every encrypted column. A new encrypted column must be added here, or rotations leave
/// its values on retired keys (the `every_bytea_enc_column_is_listed` test checks the schema).
pub const ENCRYPTED_COLUMNS: &[EncryptedColumn] = &[
    col(
        "external_services",
        "config_enc",
        RecordClass::ExternalServiceConfig,
    ),
    col(
        "user_external_accounts",
        "auth_data_enc",
        RecordClass::OAuthToken,
    ),
    col("executor_secrets", "value_enc", RecordClass::ExecutorSecret),
    EncryptedColumn {
        version_column: Some("encryption_key_version"),
        ..col(
            "user_credentials",
            "credential_enc",
            RecordClass::UserCredential,
        )
    },
    EncryptedColumn {
        version_column: Some("encryption_key_version"),
        ..col(
            "site_credentials",
            "credential_enc",
            RecordClass::SiteCredential,
        )
    },
    col("outbound_webhooks", "url_enc", RecordClass::OutboundWebhook),
    col(
        "outbound_webhooks",
        "secret_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_jobs",
        "payload_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_jobs",
        "target_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_jobs",
        "secret_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_logs",
        "request_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_logs",
        "response_enc",
        RecordClass::OutboundWebhook,
    ),
    col(
        "outbound_webhook_logs",
        "error_enc",
        RecordClass::OutboundWebhook,
    ),
    col("cm_actions", "url_enc", RecordClass::CodeMonitorWebhook),
    col("cm_actions", "header_enc", RecordClass::CodeMonitorWebhook),
];

/// SQL condition: the value is encrypted with a data key version other than `$1`.
fn stale(column: &str) -> String {
    format!("{column} IS NOT NULL AND substring({column} FROM 2 FOR 4) <> int4send($1::integer)")
}

/// Counts of one column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColumnCounts {
    /// Encrypted values.
    pub total: i64,
    /// Values not on the class's active key.
    pub stale: i64,
}

/// Out-of-band migration 1: re-encrypt secrets with the current data keys.
pub struct ReencryptSecrets {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    columns: &'static [EncryptedColumn],
    /// Rows that failed to re-encrypt (skipped by later batches of this process, so one bad
    /// value does not block the rest).
    failed: Mutex<HashSet<(&'static str, &'static str, i64)>>,
}

impl ReencryptSecrets {
    /// The migration over [`ENCRYPTED_COLUMNS`]. Without `keyring`, `up` fails with a clear
    /// error while there is work.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>) -> Self {
        Self {
            db,
            keyring,
            columns: ENCRYPTED_COLUMNS,
            failed: Mutex::new(HashSet::new()),
        }
    }

    /// The active data key version of each class, from `encryption_keys`.
    async fn active_versions(&self) -> anyhow::Result<BTreeMap<RecordClass, i32>> {
        let mut out = BTreeMap::new();
        for (name, version) in self.db.encryption_keys().active_versions().await? {
            if let Ok(class) = name.parse::<RecordClass>() {
                out.insert(class, version);
            }
        }
        Ok(out)
    }

    /// Per-column counts (columns of classes without an active key count as current).
    pub async fn counts(&self) -> anyhow::Result<Vec<(EncryptedColumn, ColumnCounts)>> {
        let active = self.active_versions().await?;
        let mut out = Vec::with_capacity(self.columns.len());
        for c in self.columns {
            let (total, stale_n): (i64, i64) = count_column(&self.db, c, active.get(&c.class))
                .await
                .with_context(|| format!("counting {}.{}", c.table, c.column))?;
            out.push((
                *c,
                ColumnCounts {
                    total,
                    stale: stale_n,
                },
            ));
        }
        Ok(out)
    }

    fn keyring(&self) -> anyhow::Result<&Keyring> {
        self.keyring.as_deref().context(
            "encryption keys are not configured on the worker (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS); \
             secrets cannot be re-encrypted",
        )
    }
}

async fn count_column(
    db: &Db,
    c: &EncryptedColumn,
    active: Option<&i32>,
) -> sourcegull_db::Result<(i64, i64)> {
    let sql = format!(
        "SELECT count({col}), count(*) FILTER (WHERE {stale}) FROM {table}",
        col = c.column,
        stale = stale(c.column),
        table = c.table,
    );
    Ok(sourcegull_db::sqlx::query_as(&sql)
        .bind(active.copied())
        .fetch_one(db.pool())
        .await?)
}

#[async_trait]
impl Migrator for ReencryptSecrets {
    async fn progress(&self) -> anyhow::Result<f64> {
        let (total, stale_n) = self
            .counts()
            .await?
            .iter()
            .fold((0i64, 0i64), |(t, s), (_, c)| (t + c.total, s + c.stale));
        if total == 0 {
            return Ok(1.0);
        }
        #[allow(clippy::cast_precision_loss)]
        Ok((total - stale_n) as f64 / total as f64)
    }

    async fn up(&self, fence: &Fence) -> anyhow::Result<()> {
        let keyring = self.keyring()?;
        self.db
            .encryption_keys()
            .reload(keyring, None)
            .await
            .context("reloading the data keys")?;
        let mut tx = fence.begin(self.db.pool()).await?;
        let mut budget = BATCH_SIZE;
        let mut errors = Vec::new();
        let mut selected = 0usize;
        for c in self.columns {
            if budget <= 0 {
                break;
            }
            let Some(version) = keyring.current_version(c.class) else {
                continue;
            };
            let skip: Vec<i64> = self
                .failed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter(|(t, col, _)| *t == c.table && *col == c.column)
                .map(|(_, _, id)| *id)
                .collect();
            let rows: Vec<(i64, Vec<u8>)> = sourcegull_db::sqlx::query_as(&format!(
                "SELECT id, {col} FROM {table} WHERE {stale} AND id <> ALL($2)
                 ORDER BY id LIMIT $3",
                col = c.column,
                table = c.table,
                stale = stale(c.column),
            ))
            .bind(version)
            .bind(&skip)
            .bind(budget)
            .fetch_all(self.db.pool())
            .await
            .with_context(|| format!("reading {}.{}", c.table, c.column))?;
            selected += rows.len();
            budget -= i64::try_from(rows.len()).unwrap_or(budget);
            let set_version = c
                .version_column
                .map(|v| format!(", {v} = $4"))
                .unwrap_or_default();
            let update = format!(
                "UPDATE {table} SET {col} = $2{set_version} WHERE id = $1 AND {col} = $3",
                table = c.table,
                col = c.column,
            );
            for (id, old) in rows {
                let new = keyring
                    .decrypt(c.class, &old)
                    .and_then(|plain| keyring.encrypt(c.class, &plain));
                let new = match new {
                    Ok(n) => n,
                    Err(e) => {
                        self.failed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert((c.table, c.column, id));
                        errors.push(format!("{}.{} id {id}: {e}", c.table, c.column));
                        continue;
                    }
                };
                let mut q = sourcegull_db::sqlx::query(&update)
                    .bind(id)
                    .bind(new)
                    .bind(old);
                if c.version_column.is_some() {
                    q = q.bind(version);
                }
                q.execute(&mut *tx)
                    .await
                    .with_context(|| format!("updating {}.{} id {id}", c.table, c.column))?;
            }
        }
        tx.commit().await?;
        if selected == 0 {
            // Only values that failed before are left: retry them in the next batch (the keys
            // may have been fixed), recording their errors again.
            self.failed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "could not re-encrypt {} value(s): {}",
                errors.len(),
                errors.join("; ")
            )
        }
    }

    async fn down(&self, _fence: &Fence) -> anyhow::Result<()> {
        Ok(())
    }

    fn reversible(&self) -> bool {
        false
    }
}
