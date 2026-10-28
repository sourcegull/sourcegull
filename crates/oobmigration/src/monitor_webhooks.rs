//! Encryption of watch webhook URLs and email headers, out-of-band migration 2.
//!
//! Webhook, Slack and Mattermost webhook actions keep their URL, and Telegram actions their bot
//! token, in `cm_actions.url` (plaintext) or `cm_actions.url_enc` (encrypted with the
//! `code_monitor_webhook` data key, `url` empty); email actions keep their header in
//! `cm_actions.header` or `cm_actions.header_enc` the same way.
//! Rows written before encryption, or while no keys were configured, are plaintext; this
//! migration encrypts them in batches, one column pair after the other.
//!
//! - **Progress** is measured in SQL (no keys needed, so the upgrade gate can read it): the
//!   share of stored values that are encrypted. Without a `code_monitor_webhook` data key in
//!   `encryption_keys` (no keys configured) plaintext is the expected format and progress
//!   is 1.
//! - **Up** encrypts up to [`BATCH_SIZE`] plaintext values per column pair.
//! - **Down** decrypts up to [`BATCH_SIZE`] values per column pair back to plaintext, so a
//!   release that predates the encrypted column can read them after a downgrade.
//!
//! Each update is a compare-and-swap on the old value, so a concurrent edit wins.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use async_trait::async_trait;
use sourcegull_crypto::{Keyring, RecordClass};
use sourcegull_db::{Db, Fence};

use crate::Migrator;

/// Values encrypted (or decrypted) per column pair and batch.
pub const BATCH_SIZE: i64 = 500;

const CLASS: RecordClass = RecordClass::CodeMonitorWebhook;

/// The `(plaintext, encrypted)` column pairs of `cm_actions`.
const COLUMNS: [(&str, &str); 2] = [("url", "url_enc"), ("header", "header_enc")];

/// Out-of-band migration 2: encrypt watch webhook URLs.
pub struct EncryptMonitorWebhooks {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    /// Values that failed, as `(column pair index, row id)` (skipped by later batches of this
    /// process, so one bad value does not block the rest).
    failed: Mutex<HashSet<(usize, i64)>>,
}

impl EncryptMonitorWebhooks {
    /// The migration. Without `keyring`, `up` and `down` fail with a clear error while there
    /// is work.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>) -> Self {
        Self {
            db,
            keyring,
            failed: Mutex::new(HashSet::new()),
        }
    }

    /// `(encrypted, total)` stored URLs and headers.
    pub async fn counts(&self) -> anyhow::Result<(i64, i64)> {
        Ok(sourcegull_db::sqlx::query_as(
            "SELECT count(url_enc) + count(header_enc),
                 count(*) FILTER (WHERE url_enc IS NOT NULL OR url <> '')
                     + count(*) FILTER (WHERE header_enc IS NOT NULL OR header <> '')
             FROM cm_actions",
        )
        .fetch_one(self.db.pool())
        .await?)
    }

    /// The keyring, reloaded from `encryption_keys` (another process may have rotated).
    async fn keyring(&self) -> anyhow::Result<&Keyring> {
        let keyring = self.keyring.as_deref().context(
            "encryption keys are not configured on the worker (set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS); \
             code monitor webhook URLs cannot be encrypted or decrypted",
        )?;
        self.db
            .encryption_keys()
            .reload(keyring, None)
            .await
            .context("reloading the data keys")?;
        Ok(keyring)
    }

    /// The rows of column pair `pair` that failed.
    fn skipped(&self, pair: usize) -> Vec<i64> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(p, _)| *p == pair)
            .map(|(_, id)| *id)
            .collect()
    }

    /// Records the failures of a batch; when nothing but failed rows was left, forgets them so
    /// the next batch retries them (the keys may have been fixed).
    fn finish(&self, selected: usize, errors: Vec<(usize, i64, String)>) -> anyhow::Result<()> {
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        if selected == 0 {
            failed.clear();
        }
        failed.extend(errors.iter().map(|(pair, id, _)| (*pair, *id)));
        drop(failed);
        if errors.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = errors
            .iter()
            .map(|(pair, id, e)| format!("cm_actions.{} id {id}: {e}", COLUMNS[*pair].0))
            .collect();
        anyhow::bail!(
            "could not migrate {} watch action secret(s): {}",
            errors.len(),
            list.join("; ")
        )
    }
}

#[async_trait]
impl Migrator for EncryptMonitorWebhooks {
    async fn progress(&self) -> anyhow::Result<f64> {
        let keyed = self
            .db
            .encryption_keys()
            .active_versions()
            .await?
            .iter()
            .any(|(name, _)| name == CLASS.as_str());
        let (encrypted, total) = self.counts().await?;
        if total == 0 || (!keyed && encrypted == 0) {
            return Ok(1.0);
        }
        #[allow(clippy::cast_precision_loss)]
        Ok(encrypted as f64 / total as f64)
    }

    async fn up(&self, fence: &Fence) -> anyhow::Result<()> {
        let keyring = self.keyring().await?;
        let mut selected = 0;
        let mut errors = Vec::new();
        let mut tx = fence.begin(self.db.pool()).await?;
        for (pair, (plain, enc_col)) in COLUMNS.into_iter().enumerate() {
            let rows: Vec<(i64, String)> = sourcegull_db::sqlx::query_as(&format!(
                "SELECT id, {plain} FROM cm_actions WHERE {enc_col} IS NULL AND {plain} <> ''
                     AND id <> ALL($1)
                 ORDER BY id LIMIT $2"
            ))
            .bind(self.skipped(pair))
            .bind(BATCH_SIZE)
            .fetch_all(self.db.pool())
            .await
            .with_context(|| format!("reading cm_actions.{plain}"))?;
            selected += rows.len();
            for (id, value) in rows {
                let enc = match keyring.encrypt(CLASS, value.as_bytes()) {
                    Ok(e) => e,
                    Err(e) => {
                        errors.push((pair, id, e.to_string()));
                        continue;
                    }
                };
                sourcegull_db::sqlx::query(&format!(
                    "UPDATE cm_actions SET {plain} = '', {enc_col} = $2
                     WHERE id = $1 AND {plain} = $3 AND {enc_col} IS NULL"
                ))
                .bind(id)
                .bind(enc)
                .bind(value)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("updating cm_actions.{plain} id {id}"))?;
            }
        }
        tx.commit().await?;
        self.finish(selected, errors)
    }

    async fn down(&self, fence: &Fence) -> anyhow::Result<()> {
        let (encrypted, _) = self.counts().await?;
        if encrypted == 0 {
            return Ok(());
        }
        let keyring = self.keyring().await?;
        let mut selected = 0;
        let mut errors = Vec::new();
        let mut tx = fence.begin(self.db.pool()).await?;
        for (pair, (plain, enc_col)) in COLUMNS.into_iter().enumerate() {
            let rows: Vec<(i64, Vec<u8>)> = sourcegull_db::sqlx::query_as(&format!(
                "SELECT id, {enc_col} FROM cm_actions WHERE {enc_col} IS NOT NULL
                     AND id <> ALL($1)
                 ORDER BY id LIMIT $2"
            ))
            .bind(self.skipped(pair))
            .bind(BATCH_SIZE)
            .fetch_all(self.db.pool())
            .await
            .with_context(|| format!("reading cm_actions.{enc_col}"))?;
            selected += rows.len();
            for (id, enc) in rows {
                let value = keyring
                    .decrypt(CLASS, &enc)
                    .map_err(|e| e.to_string())
                    .and_then(|p| {
                        String::from_utf8(p.to_vec()).map_err(|_| "not UTF-8".to_owned())
                    });
                let value = match value {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push((pair, id, e));
                        continue;
                    }
                };
                sourcegull_db::sqlx::query(&format!(
                    "UPDATE cm_actions SET {plain} = $2, {enc_col} = NULL
                     WHERE id = $1 AND {enc_col} = $3"
                ))
                .bind(id)
                .bind(value)
                .bind(enc)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("updating cm_actions.{enc_col} id {id}"))?;
            }
        }
        tx.commit().await?;
        self.finish(selected, errors)
    }
}
