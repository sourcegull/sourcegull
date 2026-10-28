//! Schema migrations for the `main` set.
//!
//! Migrations are embedded from `crates/db/migrations/main/` at build time and
//! tracked by sqlx in the `_sqlx_migrations` table. Applying is serialized
//! across processes with a Postgres advisory lock (sqlx's default).

use crate::Result;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use std::collections::BTreeMap;

/// The embedded `main` migration set.
pub static MAIN: Migrator = sqlx::migrate!("./migrations/main");

/// The state of one migration version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// Version number (the file name prefix).
    pub version: i64,
    /// Description (the file name after the version).
    pub description: String,
    /// Whether the migration is recorded as applied.
    pub applied: bool,
    /// When it was applied.
    pub applied_at: Option<DateTime<Utc>>,
    /// `false` when the applied checksum differs from the embedded file, or
    /// the applied version is unknown to this binary. `true` when unapplied.
    pub checksum_ok: bool,
    /// `true` when the migration started but did not finish (failed midway).
    pub dirty: bool,
    /// `false` when the database has this version but this binary does not
    /// (the database is newer than the code).
    pub known: bool,
}

/// A problem found by [`validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationProblem {
    /// An applied migration's file was edited after it was applied.
    ChecksumMismatch { version: i64 },
    /// The database has a version this binary does not know.
    UnknownApplied { version: i64 },
    /// A migration failed partway and needs manual repair.
    Dirty { version: i64 },
}

/// Applies every pending migration.
pub async fn up(pool: &PgPool) -> Result<()> {
    let mut conn = schema_connection(pool).await?;
    MAIN.run(&mut *conn).await?;
    Ok(())
}

/// A connection for schema changes: no `statement_timeout` (data updates on a large install
/// can take longer than the pool's), and a `lock_timeout` so a migration waiting for a table
/// lock behind long queries fails instead of queueing every other query on that table. The
/// connection is closed afterwards rather than returned to the pool with these settings.
async fn schema_connection(pool: &PgPool) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    let mut conn = pool.acquire().await?;
    conn.close_on_drop();
    sqlx::raw_sql("SET statement_timeout = 0; SET lock_timeout = '60s'")
        .execute(&mut *conn)
        .await?;
    Ok(conn)
}

/// Applies the pending migrations up to and including `target_version`, oldest first, under
/// the same advisory lock as [`up`]; returns the versions applied. Fails like [`up`] on a
/// dirty or edited history.
pub async fn up_to(pool: &PgPool, target_version: i64) -> Result<Vec<i64>> {
    use sqlx::migrate::{Migrate as _, MigrateError};
    let mut conn = schema_connection(pool).await?;
    conn.lock().await?;
    let result = async {
        conn.ensure_migrations_table().await?;
        if let Some(version) = conn.dirty_version().await? {
            return Err(MigrateError::Dirty(version));
        }
        let applied: BTreeMap<i64, Vec<u8>> = conn
            .list_applied_migrations()
            .await?
            .into_iter()
            .map(|m| (m.version, m.checksum.into_owned()))
            .collect();
        let mut done = Vec::new();
        for m in MAIN.iter().filter(|m| m.migration_type.is_up_migration()) {
            match applied.get(&m.version) {
                Some(checksum) if *checksum != *m.checksum => {
                    return Err(MigrateError::VersionMismatch(m.version));
                }
                Some(_) => {}
                None if m.version <= target_version => {
                    conn.apply(m).await?;
                    done.push(m.version);
                }
                None => {}
            }
        }
        Ok(done)
    }
    .await;
    let unlocked = conn.unlock().await;
    let done = result?;
    unlocked?;
    Ok(done)
}

/// The embedded up migrations as `(version, description)`, oldest first.
pub fn embedded() -> Vec<(i64, String)> {
    MAIN.iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| (m.version, m.description.to_string()))
        .collect()
}

/// Reverts applied migrations newer than `target_version` (newest first).
/// `down(pool, 0)` reverts everything.
pub async fn down(pool: &PgPool, target_version: i64) -> Result<()> {
    let mut conn = schema_connection(pool).await?;
    MAIN.undo(&mut *conn, target_version).await?;
    Ok(())
}

/// The newest version embedded in this binary.
pub fn latest_version() -> Option<i64> {
    MAIN.iter()
        .filter(|m| m.migration_type.is_up_migration())
        .map(|m| m.version)
        .max()
}

#[derive(sqlx::FromRow)]
struct AppliedRow {
    version: i64,
    description: String,
    installed_on: DateTime<Utc>,
    success: bool,
    checksum: Vec<u8>,
}

async fn applied(pool: &PgPool) -> Result<Vec<AppliedRow>> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as(
        "SELECT version, description, installed_on, success, checksum
         FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Lists every known migration plus any applied version this binary does not
/// know, ordered by version.
pub async fn status(pool: &PgPool) -> Result<Vec<MigrationStatus>> {
    let mut applied: BTreeMap<i64, AppliedRow> = applied(pool)
        .await?
        .into_iter()
        .map(|r| (r.version, r))
        .collect();
    let mut out = Vec::new();
    for m in MAIN.iter().filter(|m| m.migration_type.is_up_migration()) {
        let row = applied.remove(&m.version);
        out.push(MigrationStatus {
            version: m.version,
            description: m.description.to_string(),
            applied: row.is_some(),
            applied_at: row.as_ref().map(|r| r.installed_on),
            checksum_ok: row.as_ref().is_none_or(|r| r.checksum == *m.checksum),
            dirty: row.as_ref().is_some_and(|r| !r.success),
            known: true,
        });
    }
    out.extend(applied.into_values().map(|r| MigrationStatus {
        version: r.version,
        description: r.description,
        applied: true,
        applied_at: Some(r.installed_on),
        checksum_ok: false,
        dirty: !r.success,
        known: false,
    }));
    out.sort_by_key(|s| s.version);
    Ok(out)
}

/// Checks applied migrations against the embedded ones. An empty result means
/// the database history is consistent with this binary (pending migrations are
/// not problems).
pub async fn validate(pool: &PgPool) -> Result<Vec<MigrationProblem>> {
    let mut problems = Vec::new();
    for s in status(pool).await?.into_iter().filter(|s| s.applied) {
        let version = s.version;
        if s.dirty {
            problems.push(MigrationProblem::Dirty { version });
        }
        if !s.known {
            problems.push(MigrationProblem::UnknownApplied { version });
        } else if !s.checksum_ok {
            problems.push(MigrationProblem::ChecksumMismatch { version });
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for (version, description) in embedded() {
            if let Some(other) = seen.insert(version, description.clone()) {
                panic!("migrations {other:?} and {description:?} share version {version}");
            }
        }
    }
}
