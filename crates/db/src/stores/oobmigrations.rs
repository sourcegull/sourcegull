//! `out_of_band_migrations` store: registered out-of-band migrations, their progress and
//! errors. The registry and the runner are in `sourcegull-oobmigration`.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::{DbError, Result};

/// How many errors are kept per migration.
pub const MAX_MIGRATION_ERRORS: i64 = 100;

/// A release as `major.minor`, the granularity of out-of-band migration versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MinorVersion {
    /// Major version.
    pub major: i32,
    /// Minor version.
    pub minor: i32,
}

impl MinorVersion {
    /// `major.minor`.
    pub const fn new(major: i32, minor: i32) -> Self {
        Self { major, minor }
    }

    /// Parses `0.2`, `0.2.1`, `v0.2.0` or `0.2.0+dev` (patch and suffixes are ignored).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split(['+', '-']).next()?;
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        if let Some(patch) = parts.next() {
            patch.parse::<u32>().ok()?;
        }
        Some(Self { major, minor })
    }
}

impl std::fmt::Display for MinorVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The registry's description of a migration (what [`OutOfBandMigrationStore::register`]
/// writes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationDescriptor {
    /// Stable id (never reused).
    pub id: i32,
    /// Owning team.
    pub team: &'static str,
    /// Component whose data is migrated.
    pub component: &'static str,
    /// What it does.
    pub description: &'static str,
    /// Release that introduced it.
    pub introduced: MinorVersion,
    /// Release from which it must have completed.
    pub deprecated: Option<MinorVersion>,
    /// Whether old-format readers keep working while it runs.
    pub non_destructive: bool,
}

/// One `out_of_band_migrations` row.
#[derive(Debug, Clone, PartialEq)]
pub struct OutOfBandMigration {
    /// Id.
    pub id: i32,
    /// Owning team.
    pub team: String,
    /// Component.
    pub component: String,
    /// Description.
    pub description: String,
    /// Release that introduced it.
    pub introduced: MinorVersion,
    /// Release from which it must have completed.
    pub deprecated: Option<MinorVersion>,
    /// Fraction migrated (0..1).
    pub progress: f64,
    /// When it was registered.
    pub created: DateTime<Utc>,
    /// When the progress last changed.
    pub last_updated: Option<DateTime<Utc>>,
    /// Whether old-format readers keep working while it runs.
    pub non_destructive: bool,
    /// Always false.
    pub is_enterprise: bool,
    /// Whether it runs in reverse.
    pub apply_reverse: bool,
}

impl OutOfBandMigration {
    /// Whether the data is fully in the new format (forward and finished).
    pub fn complete(&self) -> bool {
        !self.apply_reverse && self.progress >= 1.0
    }
}

/// One recorded error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutOfBandMigrationError {
    /// The message.
    pub message: String,
    /// When it happened.
    pub created: DateTime<Utc>,
}

type Row = (
    i32,
    String,
    String,
    String,
    i32,
    i32,
    Option<i32>,
    Option<i32>,
    f64,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    bool,
    bool,
    bool,
);

const COLUMNS: &str = "id, team, component, description, introduced_version_major,
    introduced_version_minor, deprecated_version_major, deprecated_version_minor, progress,
    created, last_updated, non_destructive, is_enterprise, apply_reverse";

fn from_row(r: Row) -> OutOfBandMigration {
    OutOfBandMigration {
        id: r.0,
        team: r.1,
        component: r.2,
        description: r.3,
        introduced: MinorVersion::new(r.4, r.5),
        deprecated: r
            .6
            .zip(r.7)
            .map(|(major, minor)| MinorVersion::new(major, minor)),
        progress: r.8,
        created: r.9,
        last_updated: r.10,
        non_destructive: r.11,
        is_enterprise: r.12,
        apply_reverse: r.13,
    }
}

/// Reads and updates `out_of_band_migrations` and `out_of_band_migrations_errors`.
#[derive(Debug, Clone)]
pub struct OutOfBandMigrationStore {
    pool: PgPool,
}

impl OutOfBandMigrationStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Inserts the migration, or updates its description and versions (progress, direction
    /// and errors are kept). `initial_progress` applies to a new row only. Returns whether
    /// the row was new.
    pub async fn register(&self, d: &MigrationDescriptor, initial_progress: f64) -> Result<bool> {
        let new: bool = sqlx::query_scalar(
            "INSERT INTO out_of_band_migrations (id, team, component, description,
                 introduced_version_major, introduced_version_minor, deprecated_version_major,
                 deprecated_version_minor, progress, non_destructive, is_enterprise, last_updated)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, false,
                 CASE WHEN $9 > 0 THEN now() END)
             ON CONFLICT (id) DO UPDATE SET team = EXCLUDED.team,
                 component = EXCLUDED.component, description = EXCLUDED.description,
                 introduced_version_major = EXCLUDED.introduced_version_major,
                 introduced_version_minor = EXCLUDED.introduced_version_minor,
                 deprecated_version_major = EXCLUDED.deprecated_version_major,
                 deprecated_version_minor = EXCLUDED.deprecated_version_minor,
                 non_destructive = EXCLUDED.non_destructive
             RETURNING (xmax = 0)",
        )
        .bind(d.id)
        .bind(d.team)
        .bind(d.component)
        .bind(d.description)
        .bind(d.introduced.major)
        .bind(d.introduced.minor)
        .bind(d.deprecated.map(|v| v.major))
        .bind(d.deprecated.map(|v| v.minor))
        .bind(initial_progress.clamp(0.0, 1.0))
        .bind(d.non_destructive)
        .fetch_one(&self.pool)
        .await?;
        Ok(new)
    }

    /// Every registered migration, by id.
    pub async fn list(&self) -> Result<Vec<OutOfBandMigration>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM out_of_band_migrations ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }

    /// One migration.
    pub async fn get(&self, id: i32) -> Result<Option<OutOfBandMigration>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM out_of_band_migrations WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(from_row))
    }

    /// Records a newly measured progress; `last_updated` moves only when it changed. Fails
    /// with [`crate::DbError::Fenced`] when `fence` is no longer current.
    pub async fn update_progress(
        &self,
        id: i32,
        progress: f64,
        fence: &crate::Fence,
    ) -> Result<()> {
        let mut tx = fence.begin(&self.pool).await?;
        sqlx::query(
            "UPDATE out_of_band_migrations
             SET progress = $2, last_updated = now()
             WHERE id = $1 AND progress <> $2",
        )
        .bind(id)
        .bind(progress.clamp(0.0, 1.0))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records an error, keeping the newest [`MAX_MIGRATION_ERRORS`].
    pub async fn add_error(&self, id: i32, message: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO out_of_band_migrations_errors (migration_id, message) VALUES ($1, $2)",
        )
        .bind(id)
        .bind(message)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM out_of_band_migrations_errors WHERE migration_id = $1 AND id NOT IN (
                 SELECT id FROM out_of_band_migrations_errors WHERE migration_id = $1
                 ORDER BY created DESC, id DESC LIMIT $2)",
        )
        .bind(id)
        .bind(MAX_MIGRATION_ERRORS)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A migration's errors, newest first.
    pub async fn errors(&self, id: i32) -> Result<Vec<OutOfBandMigrationError>> {
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT message, created FROM out_of_band_migrations_errors
             WHERE migration_id = $1 ORDER BY created DESC, id DESC",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(message, created)| OutOfBandMigrationError { message, created })
            .collect())
    }

    /// Sets the direction the runner applies the migration in.
    pub async fn set_direction(&self, id: i32, apply_reverse: bool) -> Result<()> {
        let n = sqlx::query("UPDATE out_of_band_migrations SET apply_reverse = $2 WHERE id = $1")
            .bind(id)
            .bind(apply_reverse)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(DbError::NotFound("out-of-band migration"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minor_versions() {
        let v = MinorVersion::new;
        assert_eq!(MinorVersion::parse("0.2"), Some(v(0, 2)));
        assert_eq!(MinorVersion::parse("v1.12.3"), Some(v(1, 12)));
        assert_eq!(MinorVersion::parse("0.1.0+dev"), Some(v(0, 1)));
        assert_eq!(MinorVersion::parse("0.3.0-rc.1"), Some(v(0, 3)));
        assert_eq!(MinorVersion::parse("migration 20261119000001"), None);
        assert_eq!(MinorVersion::parse("1"), None);
        assert_eq!(MinorVersion::parse("1.x"), None);
        assert!(v(0, 9) < v(0, 10) && v(0, 10) < v(1, 0));
        assert_eq!(v(2, 5).to_string(), "2.5");
    }
}
