//! The janitor: deletes authorization requests and codes a day after they expire, registered
//! clients that never got a grant, and metadata document clients without a live grant. A
//! leader-elected singleton of the worker role.

use std::time::Duration;

use chrono::Utc;
use sourcegull_db::Db;
use tokio_util::sync::CancellationToken;

use crate::clients::{UNUSED_CLIENT_TTL, UNUSED_METADATA_CLIENT_TTL};

/// Lease name of the janitor.
pub const JANITOR: &str = "oauth-janitor";
/// How often the janitor runs.
pub const JANITOR_INTERVAL: Duration = Duration::from_secs(3600);
/// How long expired requests and codes are kept; a used code presented again in this time
/// revokes the tokens issued for it.
pub const EXPIRED_AUTHORIZATION_RETENTION: chrono::Duration = chrono::Duration::days(1);

/// What one pass deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub authorizations: u64,
    pub registered_clients: u64,
    pub metadata_clients: u64,
}

/// Deletes expired authorizations and unused clients once.
pub async fn clean(db: &Db) -> Result<Cleaned, sourcegull_db::DbError> {
    let store = db.oauth();
    let now = Utc::now();
    Ok(Cleaned {
        authorizations: store
            .delete_expired_authorizations(now - EXPIRED_AUTHORIZATION_RETENTION)
            .await?,
        registered_clients: store
            .delete_unused_registered_clients(now - UNUSED_CLIENT_TTL)
            .await?,
        metadata_clients: store
            .delete_unused_metadata_clients(now - UNUSED_METADATA_CLIENT_TTL)
            .await?,
    })
}

/// Runs [`clean`] every [`JANITOR_INTERVAL`] (first run right away) until `cancel` fires.
pub async fn run_janitor(db: Db, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(JANITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(JANITOR, clean(&db)).await {
            Ok(c) if c != Cleaned::default() => tracing::info!(
                authorizations = c.authorizations,
                registered_clients = c.registered_clients,
                metadata_clients = c.metadata_clients,
                "deleted expired OAuth authorizations and unused clients"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "deleting expired OAuth data failed"),
        }
    }
}
