//! The out-of-band migration gate: a release that deprecates a migration no longer
//! reads the old data format, so the schema must not move to it, and it must not start,
//! until that migration has completed.
//!
//! A migration counts as complete when its row says progress 1 in the forward direction, or,
//! failing that, when measuring it now gives 1 (e.g. nothing was ever encrypted with an old
//! key, or the row does not exist yet because the table is new).

use std::fmt;
use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use sourcegull_db::sqlx::PgPool;
use sourcegull_db::upgrade::{Catalog, OutOfBandGate, UpgradeStep};
use sourcegull_db::{Db, Fence, MigrationDescriptor, MinorVersion, OutOfBandMigration};

use crate::{OOB_MIGRATION_RUNNER, Registry, current_version};

/// A migration that blocks a version.
#[derive(Debug, Clone)]
pub struct Unfinished {
    /// The migration.
    pub descriptor: MigrationDescriptor,
    /// Its progress (measured now, else the stored one; `None` if neither is known).
    pub progress: Option<f64>,
    /// Whether it is being applied in reverse.
    pub apply_reverse: bool,
    /// Why progress could not be measured, if it could not.
    pub error: Option<String>,
}

impl fmt::Display for Unfinished {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = &self.descriptor;
        write!(f, "out-of-band migration {} ({})", d.id, d.description)?;
        if let Some(v) = d.deprecated {
            write!(f, ", deprecated in {v},")?;
        }
        match self.progress {
            Some(p) => write!(f, " is {:.0}% done", (p * 100.0).floor())?,
            None => write!(f, " has not run")?,
        }
        if self.apply_reverse {
            write!(f, " and is being applied in reverse")?;
        }
        if let Some(e) = &self.error {
            write!(f, " ({e})")?;
        }
        Ok(())
    }
}

/// The registry's migrations deprecated at or before `version` that have not completed.
pub async fn unfinished(
    pool: &PgPool,
    registry: &Registry,
    version: MinorVersion,
) -> sourcegull_db::Result<Vec<Unfinished>> {
    let relevant: Vec<_> = registry
        .iter()
        .filter(|m| m.descriptor.deprecated.is_some_and(|d| d <= version))
        .collect();
    if relevant.is_empty() {
        return Ok(Vec::new());
    }
    let store = Db::from_pool(pool.clone()).oob_migrations();
    let table_exists: bool = sourcegull_db::sqlx::query_scalar(
        "SELECT to_regclass('out_of_band_migrations') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    let rows: Vec<OutOfBandMigration> = if table_exists {
        store.list().await?
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    for m in relevant {
        let row = rows.iter().find(|r| r.id == m.descriptor.id);
        if row.is_some_and(OutOfBandMigration::complete) {
            continue;
        }
        let apply_reverse = row.is_some_and(|r| r.apply_reverse);
        let (progress, error) = if apply_reverse {
            (row.map(|r| r.progress), None)
        } else {
            match m.migrator.progress().await {
                Ok(p) if p >= 1.0 => {
                    if row.is_some() {
                        store
                            .update_progress(
                                m.descriptor.id,
                                1.0,
                                &Fence::none(OOB_MIGRATION_RUNNER),
                            )
                            .await?;
                    }
                    continue;
                }
                Ok(p) => (Some(p), None),
                Err(e) => (row.map(|r| r.progress), Some(format!("{e:#}"))),
            }
        };
        out.push(Unfinished {
            descriptor: m.descriptor.clone(),
            progress,
            apply_reverse,
            error,
        });
    }
    Ok(out)
}

/// Refuses to start this version while a migration it deprecates is unfinished.
pub async fn check_startup(db: &Db, registry: &Registry) -> anyhow::Result<()> {
    let version = current_version();
    let blocking = unfinished(db.pool(), registry, version).await?;
    if blocking.is_empty() {
        return Ok(());
    }
    bail!(
        "this version ({version}) needs out-of-band migrations that have not finished: {}. \
         Run the previous version until they complete (Site admin > Migrations shows their \
         progress), then upgrade again.",
        list(&blocking)
    )
}

fn list(blocking: &[Unfinished]) -> String {
    blocking
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The [`OutOfBandGate`] of `sourcegull migrate upgrade`.
pub struct OutOfBandMigrationGate {
    registry: Arc<Registry>,
    catalog: Catalog,
    empty_database: bool,
}

impl OutOfBandMigrationGate {
    /// A gate over `registry`; `catalog` maps bare migration numbers to versions.
    pub fn new(registry: Arc<Registry>, catalog: Catalog) -> Self {
        Self {
            registry,
            catalog,
            empty_database: false,
        }
    }

    /// Upgrading an empty database: there is no data to migrate, so nothing blocks.
    #[must_use]
    pub fn empty_database(mut self, empty: bool) -> Self {
        self.empty_database = empty;
        self
    }

    /// The release a step reaches: its own name, or the first release that contains its
    /// last migration, or this binary's version.
    pub fn step_version(&self, step: &UpgradeStep) -> MinorVersion {
        MinorVersion::parse(&step.version)
            .or_else(|| {
                self.catalog
                    .versions
                    .iter()
                    .find(|v| v.last_migration >= step.last_migration)
                    .and_then(|v| MinorVersion::parse(&v.name))
            })
            .unwrap_or_else(current_version)
    }
}

#[async_trait]
impl OutOfBandGate for OutOfBandMigrationGate {
    async fn before_step(&self, pool: &PgPool, step: &UpgradeStep) -> Result<(), String> {
        if self.empty_database {
            return Ok(());
        }
        let version = self.step_version(step);
        let blocking = unfinished(pool, &self.registry, version)
            .await
            .map_err(|e| format!("checking out-of-band migrations: {e}"))?;
        if blocking.is_empty() {
            return Ok(());
        }
        Err(format!(
            "{} must complete before {version}. Upgrade to the last version before {version}, \
             start it and wait until Site admin > Migrations shows them at 100%, then continue",
            list(&blocking)
        ))
    }
}

impl fmt::Debug for OutOfBandMigrationGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutOfBandMigrationGate")
            .field("registry", &self.registry)
            .field("empty_database", &self.empty_database)
            .finish_non_exhaustive()
    }
}
