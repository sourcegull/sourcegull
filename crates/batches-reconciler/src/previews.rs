//! Deletion of bulk change previews nobody applied.
//!
//! [`run_previews_janitor`] runs as a leader-elected singleton in the worker role. Every
//! [`PREVIEWS_INTERVAL`] it deletes the bulk change specs older than
//! [`BATCH_SPEC_TTL`] that no bulk change uses (with their workspaces, executions and changeset
//! specs), then the changeset specs older than [`CHANGESET_SPEC_TTL`] that were never attached
//! to a bulk change spec and no changeset uses. The retentions are the `expiresAt` times the API
//! shows for the specs.

use std::time::Duration;

use sourcegull_batches::store::{
    BATCH_SPEC_TTL, BatchSpecStore, CHANGESET_SPEC_TTL, ChangesetSpecStore,
};
use sourcegull_db::Db;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

/// Lease name of the previews janitor singleton.
pub const PREVIEWS_JANITOR: &str = "batches-previews-janitor";
/// How often the janitor runs.
pub const PREVIEWS_INTERVAL: Duration = Duration::from_secs(3600);

/// What one run deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewsDeleted {
    pub batch_specs: u64,
    pub changeset_specs: u64,
}

/// Deletes unapplied bulk change specs older than `batch_spec_ttl` and unattached changeset
/// specs older than `changeset_spec_ttl`.
pub async fn delete_expired_previews(
    db: &Db,
    batch_spec_ttl: Duration,
    changeset_spec_ttl: Duration,
) -> Result<PreviewsDeleted> {
    let batch_specs = BatchSpecStore::new(db)
        .delete_unapplied_older_than(batch_spec_ttl)
        .await?;
    let changeset_specs = ChangesetSpecStore::new(db)
        .delete_unattached_older_than(changeset_spec_ttl)
        .await?;
    Ok(PreviewsDeleted {
        batch_specs,
        changeset_specs,
    })
}

/// Deletes expired previews every [`PREVIEWS_INTERVAL`] (first right away) until `cancel`
/// fires.
pub async fn run_previews_janitor(db: Db, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(PREVIEWS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let run = delete_expired_previews(&db, BATCH_SPEC_TTL, CHANGESET_SPEC_TTL);
        match sourcegull_db::record_run(PREVIEWS_JANITOR, run).await {
            Ok(PreviewsDeleted {
                batch_specs: 0,
                changeset_specs: 0,
            }) => {}
            Ok(d) => tracing::info!(
                batch_specs = d.batch_specs,
                changeset_specs = d.changeset_specs,
                "deleted expired bulk change previews"
            ),
            Err(e) => tracing::warn!(error = %e, "deleting expired bulk change previews failed"),
        }
    }
}
