//! `sourcegull supervise`: keys, migrations and every role in one process.

use anyhow::Context as _;
use sourcegull_db::migrate;
use sourcegull_observability::MetricsHandle;
use tokio_util::sync::CancellationToken;

use crate::cli::SuperviseArgs;
use crate::roles::RoleSet;
use crate::server::run_roles;
use crate::startup::{self, DB_CONNECT_DEADLINE};

/// Runs `sourcegull supervise`: generates the key file if missing (unless the keys come from
/// `SOURCEGULL_KEYS`), applies migrations to the database at `SOURCEGULL_DATABASE_URL` and runs every
/// role.
pub async fn supervise(
    args: &SuperviseArgs,
    metrics: MetricsHandle,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let url = database_url(args.database_url.as_deref())?;
    if let startup::KeySource::File { path, .. } = args.server.key_source()? {
        startup::ensure_keys_file(&path)?;
    }

    let db = startup::connect_with_retry(
        url,
        &args.server.db_options(&RoleSet::all()),
        DB_CONNECT_DEADLINE,
        &cancel,
    )
    .await?;
    migrate::up(db.pool())
        .await
        .context("applying migrations")?;
    tracing::info!("migrations applied");
    run_roles(&args.server, &RoleSet::all(), db, metrics, cancel).await
}

/// The configured database URL, or an error that says how to set one.
fn database_url(url: Option<&str>) -> anyhow::Result<&str> {
    match url {
        Some(url) if !url.trim().is_empty() => Ok(url),
        _ => anyhow::bail!(
            "no database configured: set SOURCEGULL_DATABASE_URL or pass --database-url \
             (a PostgreSQL 18 URL, e.g. postgres://sourcegull:password@postgres:5432/sourcegull)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_url_is_required() {
        for missing in [None, Some(""), Some("  ")] {
            let err = database_url(missing).unwrap_err().to_string();
            assert!(err.contains("SOURCEGULL_DATABASE_URL"), "{err}");
            assert!(err.contains("--database-url"), "{err}");
        }
        assert_eq!(
            database_url(Some("postgres://h/db")).unwrap(),
            "postgres://h/db"
        );
    }
}
