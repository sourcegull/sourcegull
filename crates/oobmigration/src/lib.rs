//! Out-of-band migrations: long-running data migrations that run in the
//! background after the schema migrations, in batches, with their progress shown to admins.
//!
//! - A [`Migrator`] measures its progress (the fraction of the data already in the new
//!   format) and migrates one batch [`up`](Migrator::up) or [`down`](Migrator::down).
//! - The [`Registry`] lists the migrations this binary knows, each with a
//!   [`MigrationDescriptor`]: a stable id, owning team and component, and the releases that
//!   introduced and deprecated it. [`Registry::sync`] upserts them into
//!   `out_of_band_migrations` at startup.
//! - The [`runner`] (a leader-elected singleton of the `worker` role) runs each migration in
//!   the direction its row asks for and records progress and errors.
//! - The [`gate`] refuses to upgrade to (or start) a release that deprecates a migration that
//!   has not completed: from that release on, the code no longer reads the old format.
//!
//! Progress is re-measured after every batch, the newest 100 errors are kept, and
//! `applyReverse` prepares a downgrade.
//!
//! The first migration is [`reencrypt`]: after a key rotation it moves every encrypted column
//! to the current data keys. The second, [`monitor_webhooks`], encrypts watch webhook
//! URLs stored in plaintext.

mod feature;
pub mod gate;
pub mod monitor_webhooks;
pub mod reencrypt;
pub mod runner;
pub use feature::OobMigrationFeature;

use std::sync::Arc;

use async_trait::async_trait;
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, Fence, MigrationDescriptor, MinorVersion};

pub use gate::{OutOfBandMigrationGate, Unfinished, check_startup, unfinished};
pub use runner::{OOB_MIGRATION_RUNNER, Runner};

/// One out-of-band migration's implementation.
#[async_trait]
pub trait Migrator: Send + Sync {
    /// The fraction of the data in the new format, 0..=1 (1 when there is no data).
    async fn progress(&self) -> anyhow::Result<f64>;

    /// Migrates one batch forward, writing in a transaction that checks `fence` (the runner's
    /// lease).
    async fn up(&self, fence: &Fence) -> anyhow::Result<()>;

    /// Migrates one batch back to the old format, writing in a transaction that checks
    /// `fence`.
    async fn down(&self, fence: &Fence) -> anyhow::Result<()>;

    /// Whether `down` changes anything. When false, the old format is still readable after
    /// `up` (e.g. re-encryption: retired keys keep decrypting), so applying the migration in
    /// reverse just records progress 0 without touching the data.
    fn reversible(&self) -> bool {
        true
    }
}

/// A registered migration.
#[derive(Clone)]
pub struct Registered {
    /// What it is.
    pub descriptor: MigrationDescriptor,
    /// How it runs.
    pub migrator: Arc<dyn Migrator>,
}

impl std::fmt::Debug for Registered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registered")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

/// The out-of-band migrations this binary knows.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    migrations: Vec<Registered>,
}

/// Re-encrypts every encrypted column with the current data keys.
pub const REENCRYPT_SECRETS: MigrationDescriptor = MigrationDescriptor {
    id: 1,
    team: "source",
    component: "encryption",
    description: "Re-encrypt secrets with the current data keys (after a key rotation)",
    introduced: MinorVersion::new(0, 1),
    deprecated: None,
    non_destructive: true,
};

/// Encrypts watch webhook URLs and Telegram bot tokens stored in plaintext.
pub const ENCRYPT_MONITOR_WEBHOOKS: MigrationDescriptor = MigrationDescriptor {
    id: 2,
    team: "code-search",
    component: "codemonitors",
    description: "Encrypt watch webhook URLs",
    introduced: MinorVersion::new(0, 1),
    deprecated: None,
    non_destructive: true,
};

impl Registry {
    /// An empty registry (tests add their own migrations).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sourcegull's migrations. `keyring` is needed to run the encryption migrations (their
    /// progress is measured without it).
    pub fn sourcegull(db: Db, keyring: Option<Arc<Keyring>>) -> Self {
        let mut r = Self::new();
        r.register(
            REENCRYPT_SECRETS,
            Arc::new(reencrypt::ReencryptSecrets::new(
                db.clone(),
                keyring.clone(),
            )),
        );
        r.register(
            ENCRYPT_MONITOR_WEBHOOKS,
            Arc::new(monitor_webhooks::EncryptMonitorWebhooks::new(db, keyring)),
        );
        r
    }

    /// Adds a migration.
    ///
    /// # Panics
    ///
    /// If the id is already registered (a programming error).
    pub fn register(&mut self, descriptor: MigrationDescriptor, migrator: Arc<dyn Migrator>) {
        assert!(
            self.get(descriptor.id).is_none(),
            "out-of-band migration {} registered twice",
            descriptor.id
        );
        self.migrations.push(Registered {
            descriptor,
            migrator,
        });
        self.migrations.sort_by_key(|m| m.descriptor.id);
    }

    /// The migration with `id`.
    pub fn get(&self, id: i32) -> Option<&Registered> {
        self.migrations.iter().find(|m| m.descriptor.id == id)
    }

    /// Every migration, by id.
    pub fn iter(&self) -> impl Iterator<Item = &Registered> {
        self.migrations.iter()
    }

    /// Upserts every migration's row. A new row starts at the migration's measured progress
    /// (so a fresh install with nothing to migrate is complete right away), or 0 if that
    /// cannot be measured.
    pub async fn sync(&self, db: &Db) -> sourcegull_db::Result<()> {
        let store = db.oob_migrations();
        for m in &self.migrations {
            let initial = if store.get(m.descriptor.id).await?.is_some() {
                0.0
            } else {
                m.migrator.progress().await.unwrap_or(0.0)
            };
            store.register(&m.descriptor, initial).await?;
        }
        Ok(())
    }
}

/// This binary's version as `major.minor`.
pub fn current_version() -> MinorVersion {
    MinorVersion::parse(sourcegull_db::upgrade::CURRENT_VERSION).unwrap_or(MinorVersion::new(0, 0))
}
