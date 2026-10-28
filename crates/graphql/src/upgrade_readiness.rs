//! `site { upgradeReadiness }`: schema drift of the live database against the
//! version it is at, as `sourcegull migrate drift` reports it, and the out-of-band migrations that
//! must finish before the next upgrade, for the site admin's Updates page.

use async_graphql::{Context, Object, Result, SimpleObject};
use sourcegull_db::schema::Drift;
use sourcegull_db::upgrade::{self, Catalog, UpgradeError};

use crate::context::{codes, db_error, error, require_site_admin, services};

/// Whether the database is ready for an upgrade.
pub struct UpgradeReadiness {
    version: Option<String>,
    drift: Vec<Drift>,
    note: Option<String>,
}

#[Object]
impl UpgradeReadiness {
    /// Differences between the live schema and the schema of `schemaVersion` (empty when
    /// there is no drift, or no version to compare with).
    async fn schema_drift(&self) -> Vec<SchemaDriftSummary> {
        self.drift.iter().map(summary).collect()
    }

    /// Out-of-band migrations that must complete before upgrading to the next version
    /// (deprecated in it or earlier, and not finished).
    async fn required_out_of_band_migrations(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<crate::oob_migrations::OutOfBandMigrationNode>> {
        crate::oob_migrations::required(ctx).await
    }

    /// The version the schema was compared with; null when the database is not at a known
    /// version (see `schemaDriftNote`).
    async fn schema_version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Why the schema could not be compared, if it could not.
    async fn schema_drift_note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

/// One schema difference.
#[derive(SimpleObject)]
pub struct SchemaDriftSummary {
    /// The object, e.g. `column "repo.name"`.
    name: String,
    /// What is wrong.
    problem: String,
    /// What to do about it.
    solution: String,
    /// The live definition (`-`) against the expected one (`+`), when both exist.
    diff: Option<String>,
    /// SQL that would repair it (review before running).
    statements: Option<Vec<String>>,
    /// A link with more help (none yet).
    url_hint: Option<String>,
}

fn summary(d: &Drift) -> SchemaDriftSummary {
    let solution = if d.problem.starts_with("missing") {
        "Create it with the statements below."
    } else if d.problem.starts_with("unexpected") {
        "Drop it with the statements below, unless it was added on purpose."
    } else if d.fix.iter().any(|s| s.starts_with("--")) {
        "Repair it by hand as described below."
    } else {
        "Change it to the expected definition with the statements below."
    };
    SchemaDriftSummary {
        name: d.object.clone(),
        problem: d.problem.clone(),
        solution: solution.to_owned(),
        diff: match (&d.actual, &d.expected) {
            (Some(a), Some(e)) => Some(format!("- {a}\n+ {e}")),
            _ => None,
        },
        statements: (!d.fix.is_empty()).then(|| d.fix.clone()),
        url_hint: None,
    }
}

/// Site admins only.
pub async fn upgrade_readiness(ctx: &Context<'_>) -> Result<UpgradeReadiness> {
    require_site_admin(ctx)?;
    let svc = services(ctx)?;
    let catalog = Catalog::embedded();
    match upgrade::drift(svc.db.pool(), &catalog, None).await {
        Ok(report) => Ok(UpgradeReadiness {
            version: Some(report.version.name),
            drift: report.drifts,
            note: None,
        }),
        Err(UpgradeError::Invalid(note)) => Ok(UpgradeReadiness {
            version: None,
            drift: Vec::new(),
            note: Some(note),
        }),
        Err(UpgradeError::Db(e)) => Err(db_error(e)),
        Err(e) => Err(error(codes::INTERNAL, e.to_string())),
    }
}
