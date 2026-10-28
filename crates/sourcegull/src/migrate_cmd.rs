//! `sourcegull migrate up|down|status|validate|drift|upgrade|versions|describe`.

use std::fmt::Write as _;

use sourcegull_db::migrate::{self, MigrationProblem};
use sourcegull_db::schema;
use sourcegull_db::sqlx::PgPool;
use std::sync::Arc;

use sourcegull_db::Db;
use sourcegull_db::upgrade::{self, Catalog, UpgradeOptions, UpgradeReport};
use sourcegull_oobmigration::{OutOfBandMigrationGate, Registry};

use crate::cli::MigrateAction;

/// The outcome of a migrate action: text for stdout and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateOutcome {
    /// Human-readable report.
    pub output: String,
    /// `false` makes the process exit non-zero.
    pub success: bool,
}

/// Runs `action` against `pool`. Database errors are returned as `Err`; problems found by
/// `validate` are a successful run with `success: false`.
pub async fn run(pool: &PgPool, action: MigrateAction) -> anyhow::Result<MigrateOutcome> {
    let ok = |output| MigrateOutcome {
        output,
        success: true,
    };
    match action {
        MigrateAction::Up => {
            let before = pending(pool).await?;
            migrate::up(pool).await?;
            Ok(ok(match before {
                0 => format!("schema is up to date (version {})", current(pool).await?),
                n => format!(
                    "applied {n} migration(s); schema is now at version {}",
                    current(pool).await?
                ),
            }))
        }
        MigrateAction::Down { to } => {
            let reverted = migrate::status(pool)
                .await?
                .iter()
                .filter(|s| s.applied && s.known && s.version > to)
                .count();
            migrate::down(pool, to).await?;
            Ok(ok(format!(
                "reverted {reverted} migration(s); schema is now at version {}",
                current(pool).await?
            )))
        }
        MigrateAction::Status => Ok(ok(format!(
            "{}\n\n{}",
            status_table(pool).await?,
            oob_table(pool).await?
        ))),
        MigrateAction::Drift { version } => {
            let catalog = Catalog::embedded();
            let report = upgrade::drift(pool, &catalog, version.as_deref()).await?;
            if report.drifts.is_empty() {
                return Ok(ok(format!(
                    "no drift: the schema matches {}",
                    report.version.name
                )));
            }
            Ok(MigrateOutcome {
                output: format!(
                    "schema drift against {}: {} difference(s)\n\n{}\n{}",
                    report.version.name,
                    report.drifts.len(),
                    schema::report(&report.drifts),
                    FIX_NOTE
                ),
                success: false,
            })
        }
        MigrateAction::Upgrade {
            from,
            to,
            dry_run,
            skip_drift_check,
        } => {
            let catalog = Catalog::embedded();
            let to = to.unwrap_or_else(|| {
                catalog
                    .versions
                    .last()
                    .map_or_else(|| upgrade::CURRENT_VERSION.to_owned(), |v| v.name.clone())
            });
            // Out-of-band migrations deprecated in a release must be complete before the
            // schema moves to it (progress is measured, so no keys are needed).
            let empty = catalog.resolve(&from)?.last_migration == 0;
            let registry = Arc::new(Registry::sourcegull(Db::from_pool(pool.clone()), None));
            let gate = OutOfBandMigrationGate::new(registry, catalog.clone()).empty_database(empty);
            let report = upgrade::upgrade(
                pool,
                &catalog,
                &from,
                &to,
                UpgradeOptions {
                    dry_run,
                    skip_drift_check,
                },
                &gate,
            )
            .await?;
            Ok(MigrateOutcome {
                success: report.success(),
                output: upgrade_report(&report, dry_run),
            })
        }
        MigrateAction::Versions => {
            let catalog = Catalog::embedded();
            let mut out = format!("{:<16} {:<16} {}\n", "VERSION", "LAST MIGRATION", "SCHEMA");
            for v in &catalog.versions {
                let _ = writeln!(
                    out,
                    "{:<16} {:<16} {}",
                    v.name,
                    v.last_migration,
                    if v.description.is_some() {
                        "described"
                    } else {
                        "-"
                    }
                );
            }
            let state = upgrade::history(pool).await?;
            let at = catalog
                .version_at(state.newest)
                .map(|v| v.name.clone())
                .unwrap_or_else(|| "between versions".to_owned());
            let _ = write!(out, "\ndatabase: newest migration {} ({at})", state.newest);
            Ok(ok(out))
        }
        MigrateAction::Describe => Ok(ok(schema::describe(pool)
            .await?
            .to_json()
            .trim_end()
            .to_owned())),
        MigrateAction::Validate => {
            let problems = migrate::validate(pool).await?;
            if problems.is_empty() {
                let n = pending(pool).await?;
                return Ok(ok(format!("migration history is consistent ({n} pending)")));
            }
            let mut output = String::from("migration history has problems:\n");
            for p in &problems {
                let line = match p {
                    MigrationProblem::ChecksumMismatch { version } => format!(
                        "  {version}: applied migration differs from the one in this binary (edited after release?)"
                    ),
                    MigrationProblem::UnknownApplied { version } => format!(
                        "  {version}: applied, but unknown to this binary (the database is newer than this release)"
                    ),
                    MigrationProblem::Dirty { version } => format!(
                        "  {version}: failed partway; repair the schema manually, then fix the _sqlx_migrations row"
                    ),
                };
                output.push_str(&line);
                output.push('\n');
            }
            Ok(MigrateOutcome {
                output: output.trim_end().to_owned(),
                success: false,
            })
        }
    }
}

async fn pending(pool: &PgPool) -> anyhow::Result<usize> {
    Ok(migrate::status(pool)
        .await?
        .iter()
        .filter(|s| s.known && !s.applied)
        .count())
}

/// The newest applied version, or 0.
async fn current(pool: &PgPool) -> anyhow::Result<i64> {
    Ok(migrate::status(pool)
        .await?
        .iter()
        .filter(|s| s.applied)
        .map(|s| s.version)
        .max()
        .unwrap_or(0))
}

/// A table of every migration and its state.
pub async fn status_table(pool: &PgPool) -> anyhow::Result<String> {
    let rows = migrate::status(pool).await?;
    let mut out = format!(
        "{:<16} {:<8} {:<26} {}\n",
        "VERSION", "STATE", "APPLIED AT", "DESCRIPTION"
    );
    for s in &rows {
        let state = match (s.applied, s.dirty, s.known, s.checksum_ok) {
            (false, ..) => "pending",
            (true, true, ..) => "dirty",
            (true, _, false, _) => "unknown",
            (true, _, true, false) => "changed",
            (true, ..) => "applied",
        };
        let at = s
            .applied_at
            .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "-".to_owned());
        let _ = writeln!(
            out,
            "{:<16} {:<8} {:<26} {}",
            s.version, state, at, s.description
        );
    }
    Ok(out.trim_end().to_owned())
}

/// The out-of-band migrations and their progress, from `out_of_band_migrations`.
pub async fn oob_table(pool: &PgPool) -> anyhow::Result<String> {
    let exists: bool = sourcegull_db::sqlx::query_scalar(
        "SELECT to_regclass('out_of_band_migrations') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if !exists {
        return Ok("out-of-band migrations: none (schema not migrated yet)".to_owned());
    }
    let rows = Db::from_pool(pool.clone()).oob_migrations().list().await?;
    if rows.is_empty() {
        return Ok(
            "out-of-band migrations: none registered yet (they register at startup)".to_owned(),
        );
    }
    let mut out = format!(
        "{:<4} {:<9} {:<11} {:<11} {:<10} {}\n",
        "OOB", "PROGRESS", "INTRODUCED", "DEPRECATED", "DIRECTION", "DESCRIPTION"
    );
    for m in &rows {
        let _ = writeln!(
            out,
            "{:<4} {:<9} {:<11} {:<11} {:<10} {}",
            m.id,
            format!("{:.1}%", m.progress * 100.0),
            m.introduced.to_string(),
            m.deprecated
                .map_or_else(|| "-".to_owned(), |v| v.to_string()),
            if m.apply_reverse {
                "reverse"
            } else {
                "forward"
            },
            m.description
        );
    }
    Ok(out.trim_end().to_owned())
}

/// Printed under a drift report.
const FIX_NOTE: &str = "Review the fix SQL before running it (dropping a column loses its data). \
    Drift usually comes from manual schema changes or a failed migration repaired by hand.";

/// The report of `migrate upgrade`.
fn upgrade_report(r: &UpgradeReport, dry_run: bool) -> String {
    let mut out = format!(
        "upgrade from {} to {}{}\n",
        r.from.name,
        r.to.name,
        if dry_run { " (dry run)" } else { "" }
    );
    match &r.drift_check_skipped {
        Some(why) => {
            let _ = writeln!(out, "drift check: skipped ({why})");
        }
        None if r.drift_before.is_empty() => {
            let _ = writeln!(out, "drift check against {}: ok", r.from.name);
        }
        None => {
            let _ = write!(
                out,
                "drift check against {}: {} difference(s); nothing was changed\n\n{}\n{}\n\
                 Fix the drift, or rerun with --skip-drift-check.",
                r.from.name,
                r.drift_before.len(),
                schema::report(&r.drift_before),
                FIX_NOTE
            );
            return out;
        }
    }
    if r.steps.is_empty() {
        let _ = write!(out, "nothing to do: the database is at {}", r.to.name);
        return out;
    }
    let total: usize = r.steps.iter().map(|s| s.migrations.len()).sum();
    let _ = writeln!(
        out,
        "plan: {total} migration(s) in {} step(s)",
        r.steps.len()
    );
    for step in &r.steps {
        let _ = writeln!(out, "  to {}:", step.version);
        for (version, description) in &step.migrations {
            let _ = writeln!(out, "    {version} {description}");
        }
    }
    if !r.applied {
        return out.trim_end().to_owned();
    }
    let _ = writeln!(
        out,
        "applied {total} migration(s); the database is at {}",
        r.to.name
    );
    match &r.drift_after {
        None => {
            let _ = write!(out, "no schema description for {}; not checked", r.to.name);
        }
        Some(d) if d.is_empty() => {
            let _ = write!(out, "drift check against {}: ok", r.to.name);
        }
        Some(d) => {
            let _ = write!(
                out,
                "drift check against {}: {} difference(s)\n\n{}",
                r.to.name,
                d.len(),
                schema::report(d)
            );
        }
    }
    out.trim_end().to_owned()
}
