//! Versions, drift checks and multi-version upgrades (`sourcegull migrate drift|upgrade`).
//!
//! # Versions
//!
//! A version is a Sourcegull release (`0.1.0`, the workspace version that the images, the Helm
//! chart and Compose's `SOURCEGULL_VERSION` carry). Each released version is listed in [`RELEASES`]
//! with the newest `main` migration it shipped and the description of the schema its
//! migrations produce ([`crate::schema`]), committed as `crates/db/schema/<version>.json`.
//! The version under development (this crate's `CARGO_PKG_VERSION`, unless already released)
//! ends at the newest embedded migration; its description is `crates/db/schema/current.json`.
//! The `schema_descriptions` test checks every description against the migrations
//! (regenerate `current.json` with `SOURCEGULL_UPDATE_SCHEMA=1`), so editing a released migration
//! fails CI.
//!
//! **Releasing version X:** `scripts/release-version.sh` (run by the release workflow) copies
//! `current.json` to `X.json` and adds `release!("X", <newest migration>)` to [`RELEASES`].
//!
//! Wherever a version is accepted, a migration number is accepted too (`20261021000001`, or
//! `0` for an empty database). Unless it is a release boundary it has no description, so drift
//! checks skip it.
//!
//! # Upgrades
//!
//! [`upgrade`] checks that the database is exactly at `from` (every migration up to its last
//! one applied, nothing newer), compares the live schema with `from`'s description (drift
//! would make later migrations fail or leave the schema wrong), then applies the migrations
//! release by release up to `to`, consulting an [`OutOfBandGate`] before each release's
//! migrations, and finally compares the result with `to`'s description. Every migration since
//! the first one stays embedded (no squash yet), so any older version can be upgraded in one
//! run of the newest binary.

use std::collections::BTreeSet;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::migrate;
use crate::schema::{self, Drift, SchemaDescription};

/// A released version.
#[derive(Debug, Clone, Copy)]
pub struct Release {
    /// The version, e.g. `0.1.0`.
    pub version: &'static str,
    /// The newest `main` migration it shipped.
    pub last_migration: i64,
    /// Its schema description (JSON).
    pub description: &'static str,
}

/// Declares a release whose description is `crates/db/schema/<version>.json`.
#[allow(unused_macros)]
macro_rules! release {
    ($version:literal, $last_migration:literal) => {
        Release {
            version: $version,
            last_migration: $last_migration,
            description: include_str!(concat!("../schema/", $version, ".json")),
        }
    };
}

/// Released versions, oldest first.
pub const RELEASES: &[Release] = &[
    release!("0.1.0", 20261208000001),
    release!("0.2.0", 20261208000001),
    release!("0.3.0", 20261208000001),
];

/// The description of the schema of every embedded migration (the version under
/// development).
pub const CURRENT_DESCRIPTION: &str = include_str!("../schema/current.json");

/// This binary's version.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A point in the migration history: a release, or a bare migration number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionPoint {
    /// `0.1.0`, or `migration 20261021000001`.
    pub name: String,
    /// The newest migration applied at this point (0: none).
    pub last_migration: i64,
    /// The expected schema (JSON), when known.
    pub description: Option<String>,
}

impl VersionPoint {
    /// The expected schema, when known.
    pub fn schema(&self) -> Result<Option<SchemaDescription>, UpgradeError> {
        self.description
            .as_deref()
            .map(|json| {
                SchemaDescription::from_json(json).map_err(|e| {
                    UpgradeError::Invalid(format!("the schema description of {}: {e}", self.name))
                })
            })
            .transpose()
    }
}

/// The versions this binary can check and upgrade between.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// Oldest first.
    pub versions: Vec<VersionPoint>,
    /// The embedded migrations, oldest first.
    pub migrations: Vec<(i64, String)>,
}

impl Catalog {
    /// [`RELEASES`] plus the version under development.
    pub fn embedded() -> Self {
        let migrations = migrate::embedded();
        let newest = migrations.last().map_or(0, |m| m.0);
        let mut versions: Vec<VersionPoint> = RELEASES
            .iter()
            .map(|r| VersionPoint {
                name: r.version.to_owned(),
                last_migration: r.last_migration,
                description: Some(r.description.to_owned()),
            })
            .collect();
        let current_released = versions
            .iter()
            .any(|v| v.name == CURRENT_VERSION && v.last_migration == newest);
        if !current_released {
            let name = if versions.iter().any(|v| v.name == CURRENT_VERSION) {
                // Migrations were added after the release without bumping the version.
                format!("{CURRENT_VERSION}+dev")
            } else {
                CURRENT_VERSION.to_owned()
            };
            versions.push(VersionPoint {
                name,
                last_migration: newest,
                description: Some(CURRENT_DESCRIPTION.to_owned()),
            });
        }
        Self::new(versions, migrations)
    }

    /// A catalog of `versions` over `migrations` (tests use their own).
    pub fn new(mut versions: Vec<VersionPoint>, migrations: Vec<(i64, String)>) -> Self {
        versions.sort_by_key(|v| v.last_migration);
        Self {
            versions,
            migrations,
        }
    }

    /// The newest embedded migration.
    pub fn newest_migration(&self) -> i64 {
        self.migrations.last().map_or(0, |m| m.0)
    }

    /// The newest version whose last migration is `migration`.
    pub fn version_at(&self, migration: i64) -> Option<&VersionPoint> {
        self.versions
            .iter()
            .rev()
            .find(|v| v.last_migration == migration)
    }

    /// Resolves a version name (`0.1.0`, `v0.1.0`) or a migration number (`0` for none).
    pub fn resolve(&self, spec: &str) -> Result<VersionPoint, UpgradeError> {
        let spec = spec.trim();
        let name = spec.strip_prefix('v').unwrap_or(spec);
        if let Some(v) = self.versions.iter().rev().find(|v| v.name == name) {
            return Ok(v.clone());
        }
        let Ok(n) = spec.parse::<i64>() else {
            return Err(UpgradeError::Invalid(format!(
                "unknown version {spec:?}; known versions: {} (or a migration number)",
                self.versions
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        if let Some(v) = self.version_at(n) {
            return Ok(v.clone());
        }
        if n != 0 && !self.migrations.iter().any(|m| m.0 == n) {
            return Err(UpgradeError::Invalid(format!(
                "{n} is not a migration of this binary (newest: {})",
                self.newest_migration()
            )));
        }
        Ok(VersionPoint {
            name: if n == 0 {
                "empty database".to_owned()
            } else {
                format!("migration {n}")
            },
            last_migration: n,
            description: None,
        })
    }

    /// The steps from `from` to `to`: one per release boundary crossed, each with the
    /// migrations that lead to it.
    pub fn plan(
        &self,
        from: &VersionPoint,
        to: &VersionPoint,
    ) -> Result<Vec<UpgradeStep>, UpgradeError> {
        if to.last_migration < from.last_migration {
            return Err(UpgradeError::Invalid(format!(
                "{} is older than {}; to downgrade, use `sourcegull migrate down --to <migration>`",
                to.name, from.name
            )));
        }
        if to.last_migration > self.newest_migration() {
            return Err(UpgradeError::Invalid(format!(
                "this binary has migrations up to {} only; upgrade to {} with its own binary",
                self.newest_migration(),
                to.name
            )));
        }
        let mut boundaries: Vec<&VersionPoint> = Vec::new();
        for v in &self.versions {
            if v.last_migration > from.last_migration && v.last_migration < to.last_migration {
                match boundaries.last_mut() {
                    // Releases without migrations of their own: keep the newest name.
                    Some(last) if last.last_migration == v.last_migration => *last = v,
                    _ => boundaries.push(v),
                }
            }
        }
        boundaries.push(to);
        let mut prev = from.last_migration;
        let mut steps = Vec::new();
        for b in boundaries {
            steps.push(UpgradeStep {
                version: b.name.clone(),
                last_migration: b.last_migration,
                migrations: self
                    .migrations
                    .iter()
                    .filter(|m| m.0 > prev && m.0 <= b.last_migration)
                    .cloned()
                    .collect(),
            });
            prev = b.last_migration;
        }
        if from.last_migration == to.last_migration {
            steps.clear();
        }
        Ok(steps)
    }
}

/// One leg of an upgrade: the migrations that bring the schema to `version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeStep {
    /// The version reached at the end of the step.
    pub version: String,
    /// Its last migration.
    pub last_migration: i64,
    /// `(version, description)` of the migrations to apply, oldest first.
    pub migrations: Vec<(i64, String)>,
}

/// Out-of-band (background data) migrations that must be finished before the schema may move
/// past a version. The gate over registered migrations lives in `sourcegull-oobmigration`;
/// [`NoOutOfBandMigrations`] blocks nothing.
#[async_trait]
pub trait OutOfBandGate: Send + Sync {
    /// Called before `step`'s migrations run. An error stops the upgrade with that message,
    /// e.g. "out-of-band migration 3 (re-encrypt tokens) is 40% done; it must finish before
    /// 0.3.0".
    async fn before_step(&self, pool: &PgPool, step: &UpgradeStep) -> Result<(), String>;
}

/// The gate while no out-of-band migrations exist.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOutOfBandMigrations;

#[async_trait]
impl OutOfBandGate for NoOutOfBandMigrations {
    async fn before_step(&self, _pool: &PgPool, _step: &UpgradeStep) -> Result<(), String> {
        Ok(())
    }
}

/// Why a drift check or an upgrade could not run.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    /// Bad arguments or a database in an unexpected state.
    #[error("{0}")]
    Invalid(String),
    /// An out-of-band migration is not finished.
    #[error("blocked before {version}: {reason}")]
    Gate {
        /// The version that could not be reached.
        version: String,
        /// The gate's message.
        reason: String,
    },
    /// A database error.
    #[error(transparent)]
    Db(#[from] crate::DbError),
}

impl From<sqlx::Error> for UpgradeError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Where the database's migration history stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryState {
    /// Applied migrations this binary knows.
    pub applied: BTreeSet<i64>,
    /// The newest applied migration (0: none).
    pub newest: i64,
}

/// Reads the history and rejects states no upgrade or drift check should start from: dirty
/// or edited migrations, migrations this binary does not know.
pub async fn history(pool: &PgPool) -> Result<HistoryState, UpgradeError> {
    let problems = migrate::validate(pool).await?;
    if let Some(p) = problems.first() {
        return Err(UpgradeError::Invalid(match p {
            migrate::MigrationProblem::Dirty { version } => format!(
                "migration {version} failed partway; repair it first (see `sourcegull migrate validate`)"
            ),
            migrate::MigrationProblem::ChecksumMismatch { version } => format!(
                "applied migration {version} differs from this binary's; see `sourcegull migrate validate`"
            ),
            migrate::MigrationProblem::UnknownApplied { version } => format!(
                "the database has migration {version}, which this binary does not know: it is newer than {CURRENT_VERSION}"
            ),
        }));
    }
    let applied: BTreeSet<i64> = migrate::status(pool)
        .await?
        .into_iter()
        .filter(|s| s.applied)
        .map(|s| s.version)
        .collect();
    let newest = applied.last().copied().unwrap_or(0);
    Ok(HistoryState { applied, newest })
}

/// Fails unless exactly the migrations up to `point` are applied.
pub fn ensure_at(
    catalog: &Catalog,
    state: &HistoryState,
    point: &VersionPoint,
) -> Result<(), UpgradeError> {
    let expected: BTreeSet<i64> = catalog
        .migrations
        .iter()
        .map(|m| m.0)
        .filter(|v| *v <= point.last_migration)
        .collect();
    if state.applied == expected {
        return Ok(());
    }
    let missing: Vec<String> = expected
        .difference(&state.applied)
        .map(i64::to_string)
        .collect();
    let newer: Vec<String> = state
        .applied
        .difference(&expected)
        .map(i64::to_string)
        .collect();
    let looks_like = catalog
        .version_at(state.newest)
        .map(|v| v.name.clone())
        .unwrap_or_else(|| format!("migration {}", state.newest));
    let mut msg = format!(
        "the database is not at {} (its newest migration is {}, i.e. {looks_like})",
        point.name, state.newest
    );
    if !missing.is_empty() {
        msg.push_str(&format!("; not applied: {}", missing.join(", ")));
    }
    if !newer.is_empty() {
        msg.push_str(&format!("; applied beyond it: {}", newer.join(", ")));
    }
    Err(UpgradeError::Invalid(msg))
}

/// The result of [`drift`].
#[derive(Debug, Clone)]
pub struct DriftReport {
    /// The version compared against.
    pub version: VersionPoint,
    /// Differences; empty means no drift.
    pub drifts: Vec<Drift>,
}

/// Compares the live schema with `version`'s description, or, when `None`, with the
/// description of the version the database is at.
pub async fn drift(
    pool: &PgPool,
    catalog: &Catalog,
    version: Option<&str>,
) -> Result<DriftReport, UpgradeError> {
    let point = match version {
        Some(spec) => catalog.resolve(spec)?,
        None => {
            let state = history(pool).await?;
            catalog.version_at(state.newest).cloned().ok_or_else(|| {
                UpgradeError::Invalid(format!(
                    "the database's newest migration ({}) is not the last migration of a known \
                     version ({}); pass --version",
                    state.newest,
                    catalog
                        .versions
                        .iter()
                        .map(|v| format!("{} = {}", v.name, v.last_migration))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
        }
    };
    let Some(expected) = point.schema()? else {
        return Err(UpgradeError::Invalid(format!(
            "there is no schema description for {}; drift can be checked against: {}",
            point.name,
            catalog
                .versions
                .iter()
                .filter(|v| v.description.is_some())
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    };
    let actual = schema::describe(pool).await?;
    Ok(DriftReport {
        drifts: schema::diff(&expected, &actual),
        version: point,
    })
}

/// [`upgrade`] options.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpgradeOptions {
    /// Plan and check only; change nothing.
    pub dry_run: bool,
    /// Do not compare the schema with `from`'s description first.
    pub skip_drift_check: bool,
}

/// What [`upgrade`] did.
#[derive(Debug, Clone)]
pub struct UpgradeReport {
    /// The resolved versions.
    pub from: VersionPoint,
    /// The target.
    pub to: VersionPoint,
    /// The plan (executed unless `dry_run` or `refused`).
    pub steps: Vec<UpgradeStep>,
    /// Drift found before upgrading; when non-empty (and not skipped) nothing was changed.
    pub drift_before: Vec<Drift>,
    /// Why the drift check was skipped, if it was.
    pub drift_check_skipped: Option<String>,
    /// Whether migrations were applied.
    pub applied: bool,
    /// Drift against `to`'s description after upgrading (`None`: not checked).
    pub drift_after: Option<Vec<Drift>>,
}

impl UpgradeReport {
    /// Whether the upgrade (or the dry run's checks) succeeded.
    pub fn success(&self) -> bool {
        self.drift_before.is_empty() && self.drift_after.as_ref().is_none_or(Vec::is_empty)
    }
}

/// Upgrades the database from `from` to `to` (see the module docs).
pub async fn upgrade(
    pool: &PgPool,
    catalog: &Catalog,
    from: &str,
    to: &str,
    opts: UpgradeOptions,
    gate: &dyn OutOfBandGate,
) -> Result<UpgradeReport, UpgradeError> {
    let from = catalog.resolve(from)?;
    let to = catalog.resolve(to)?;
    let steps = catalog.plan(&from, &to)?;
    let state = history(pool).await?;
    ensure_at(catalog, &state, &from)?;

    let mut report = UpgradeReport {
        from: from.clone(),
        to: to.clone(),
        steps,
        drift_before: Vec::new(),
        drift_check_skipped: None,
        applied: false,
        drift_after: None,
    };
    if opts.skip_drift_check {
        report.drift_check_skipped = Some("--skip-drift-check".to_owned());
    } else if from.last_migration == 0 {
        report.drift_check_skipped = Some("empty database".to_owned());
    } else {
        match from.schema()? {
            Some(expected) => {
                let actual = schema::describe(pool).await?;
                report.drift_before = schema::diff(&expected, &actual);
            }
            None => {
                report.drift_check_skipped =
                    Some(format!("no schema description for {}", from.name));
            }
        }
    }
    if !report.drift_before.is_empty() || opts.dry_run {
        return Ok(report);
    }

    for step in &report.steps {
        gate.before_step(pool, step)
            .await
            .map_err(|reason| UpgradeError::Gate {
                version: step.version.clone(),
                reason,
            })?;
        migrate::up_to(pool, step.last_migration).await?;
    }
    report.applied = true;
    if let Some(expected) = to.schema()? {
        let actual = schema::describe(pool).await?;
        report.drift_after = Some(schema::diff(&expected, &actual));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let migrations = (1..=6).map(|v| (v, format!("m{v}"))).collect();
        let point = |name: &str, last| VersionPoint {
            name: name.to_owned(),
            last_migration: last,
            description: Some("{}".to_owned()),
        };
        Catalog::new(
            vec![
                point("0.3.0", 6),
                point("0.1.0", 2),
                point("0.2.0", 4),
                point("0.2.1", 4),
            ],
            migrations,
        )
    }

    fn versions(steps: &[UpgradeStep]) -> Vec<(&str, Vec<i64>)> {
        steps
            .iter()
            .map(|s| {
                (
                    s.version.as_str(),
                    s.migrations.iter().map(|m| m.0).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn resolves_names_and_migration_numbers() {
        let c = catalog();
        assert_eq!(c.resolve("v0.2.0").unwrap().last_migration, 4);
        assert_eq!(c.resolve("4").unwrap().name, "0.2.1");
        assert_eq!(c.resolve("3").unwrap().name, "migration 3");
        assert!(c.resolve("3").unwrap().description.is_none());
        assert_eq!(c.resolve("0").unwrap().name, "empty database");
        assert!(c.resolve("9").is_err());
        assert!(c.resolve("1.0.0").is_err());
    }

    #[test]
    fn plans_one_step_per_release() {
        let c = catalog();
        let r = |s: &str| c.resolve(s).unwrap();
        assert_eq!(
            versions(&c.plan(&r("0.1.0"), &r("0.3.0")).unwrap()),
            [("0.2.1", vec![3, 4]), ("0.3.0", vec![5, 6])]
        );
        assert_eq!(
            versions(&c.plan(&r("0"), &r("0.2.0")).unwrap()),
            [("0.1.0", vec![1, 2]), ("0.2.0", vec![3, 4])]
        );
        assert_eq!(
            versions(&c.plan(&r("3"), &r("5")).unwrap()),
            [("0.2.1", vec![4]), ("migration 5", vec![5])]
        );
        assert!(c.plan(&r("0.2.0"), &r("0.2.1")).unwrap().is_empty());
        assert!(c.plan(&r("0.3.0"), &r("0.1.0")).is_err());
    }

    #[test]
    fn ensure_at_explains_the_mismatch() {
        let c = catalog();
        let state = HistoryState {
            applied: [1, 2, 3].into(),
            newest: 3,
        };
        assert!(ensure_at(&c, &state, &c.resolve("3").unwrap()).is_ok());
        let err = ensure_at(&c, &state, &c.resolve("0.1.0").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("applied beyond it: 3"), "{err}");
        let err = ensure_at(&c, &state, &c.resolve("0.2.0").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not applied: 4"), "{err}");
    }

    #[test]
    fn embedded_catalog_ends_at_the_newest_migration() {
        let c = Catalog::embedded();
        let current = c.versions.last().unwrap();
        assert_eq!(current.last_migration, migrate::latest_version().unwrap());
        assert!(current.name.starts_with(CURRENT_VERSION));
        assert!(current.schema().unwrap().is_some());
    }
}
