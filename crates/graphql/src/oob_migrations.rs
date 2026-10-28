//! Out-of-band migrations in the admin API: `outOfBandMigrations`,
//! `setMigrationDirection` and `site.upgradeReadiness.requiredOutOfBandMigrations`, as
//! `OutOfBandMigration` and `OutOfBandMigrationError`. Site admins only. The
//! runner (`sourcegull-oobmigration`) keeps the rows' progress current.

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_db::{MinorVersion, OutOfBandMigration};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

use crate::EmptyResponse;
use crate::audit;
use crate::context::{codes, db_error, error, require_site_admin, services};

/// Relay node kind.
pub const OOB_MIGRATION_KIND: &str = "OutOfBandMigration";

/// Audit event of `setMigrationDirection`.
const DIRECTION_SET_EVENT: &str = "OutOfBandMigrationDirectionSet";

/// A background data migration (`OutOfBandMigration`).
pub struct OutOfBandMigrationNode(pub OutOfBandMigration);

#[Object(name = "OutOfBandMigration")]
impl OutOfBandMigrationNode {
    /// The node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(OOB_MIGRATION_KIND, i64::from(self.0.id)))
    }

    /// The team that owns the migration.
    async fn team(&self) -> &str {
        &self.0.team
    }

    /// The component whose data is migrated.
    async fn component(&self) -> &str {
        &self.0.component
    }

    /// What the migration does.
    async fn description(&self) -> &str {
        &self.0.description
    }

    /// The version (`major.minor`) that introduced the migration.
    async fn introduced(&self) -> String {
        self.0.introduced.to_string()
    }

    /// The version from which the migration must have completed; upgrades to it are refused
    /// until then. Null while not deprecated.
    async fn deprecated(&self) -> Option<String> {
        self.0.deprecated.map(|v| v.to_string())
    }

    /// The fraction of the data migrated (0 to 1).
    async fn progress(&self) -> f64 {
        self.0.progress
    }

    /// When the migration was registered.
    async fn created(&self) -> DateTime<Utc> {
        self.0.created
    }

    /// When the progress last changed.
    async fn last_updated(&self) -> Option<DateTime<Utc>> {
        self.0.last_updated
    }

    /// Whether data in the old format stays readable while the migration runs.
    async fn non_destructive(&self) -> bool {
        self.0.non_destructive
    }

    /// Whether the migration is applied in reverse (to prepare a downgrade).
    async fn apply_reverse(&self) -> bool {
        self.0.apply_reverse
    }

    /// Errors of the migration's recent batches, newest first (at most 100).
    async fn errors(&self, ctx: &Context<'_>) -> Result<Vec<OutOfBandMigrationErrorObject>> {
        let errors = services(ctx)?
            .db
            .oob_migrations()
            .errors(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(errors
            .into_iter()
            .map(|e| OutOfBandMigrationErrorObject {
                message: e.message,
                created: e.created,
            })
            .collect())
    }
}

/// An error of an out-of-band migration batch.
#[derive(SimpleObject)]
#[graphql(name = "OutOfBandMigrationError")]
pub struct OutOfBandMigrationErrorObject {
    /// The error message.
    message: String,
    /// When it happened.
    created: DateTime<Utc>,
}

/// Loads a migration by node ID (site admins only).
pub async fn load_node(ctx: &Context<'_>, id: &ID) -> Result<Option<OutOfBandMigrationNode>> {
    require_site_admin(ctx)?;
    let Some(id) = unmarshal_int_id(OOB_MIGRATION_KIND, id).and_then(|n| i32::try_from(n).ok())
    else {
        return Ok(None);
    };
    Ok(services(ctx)?
        .db
        .oob_migrations()
        .get(id)
        .await
        .map_err(db_error)?
        .map(OutOfBandMigrationNode))
}

/// Migrations that must complete before upgrading past this version: those deprecated at or
/// before the next minor version that have not completed (`requiredOutOfBandMigrations`).
pub async fn required(ctx: &Context<'_>) -> Result<Vec<OutOfBandMigrationNode>> {
    let current = MinorVersion::parse(sourcegull_db::upgrade::CURRENT_VERSION)
        .unwrap_or(MinorVersion::new(0, 0));
    let next = MinorVersion::new(current.major, current.minor + 1);
    let rows = services(ctx)?
        .db
        .oob_migrations()
        .list()
        .await
        .map_err(db_error)?;
    Ok(rows
        .into_iter()
        .filter(|m| m.deprecated.is_some_and(|d| d <= next) && !m.complete())
        .map(OutOfBandMigrationNode)
        .collect())
}

#[derive(Default)]
pub struct OobMigrationsQuery;

#[Object]
impl OobMigrationsQuery {
    /// Every registered out-of-band migration (site admins only).
    async fn out_of_band_migrations(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<OutOfBandMigrationNode>> {
        require_site_admin(ctx)?;
        Ok(services(ctx)?
            .db
            .oob_migrations()
            .list()
            .await
            .map_err(db_error)?
            .into_iter()
            .map(OutOfBandMigrationNode)
            .collect())
    }
}

#[derive(Default)]
pub struct OobMigrationsMutation;

#[Object]
impl OobMigrationsMutation {
    /// Sets the direction an out-of-band migration runs in (site admins only). Forward
    /// migrates data into the format newer versions read; in reverse it moves data back to
    /// the format of the previous version, before a downgrade. Migrations whose old format
    /// stays readable (such as re-encryption) have nothing to undo and just report 0%.
    async fn set_migration_direction(
        &self,
        ctx: &Context<'_>,
        id: ID,
        apply_reverse: bool,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let s = services(ctx)?;
        let Some(mid) =
            unmarshal_int_id(OOB_MIGRATION_KIND, &id).and_then(|n| i32::try_from(n).ok())
        else {
            return Err(error(codes::NOT_FOUND, "out-of-band migration not found"));
        };
        s.db.oob_migrations()
            .set_direction(mid, apply_reverse)
            .await
            .map_err(db_error)?;
        audit::record(
            s,
            DIRECTION_SET_EVENT,
            Some(actor.user_id),
            json!({ "id": mid, "applyReverse": apply_reverse }),
        )
        .await;
        Ok(EmptyResponse)
    }
}
