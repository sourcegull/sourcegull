//! Archived changeset retention (`batchChanges.changesetsRetention`).
//!
//! [`run_retention_janitor`] runs as a leader-elected singleton in the worker role. Every
//! [`RETENTION_INTERVAL`], when the setting is set, it deletes (with their events and bulk
//! operation jobs) the changesets no bulk change has used for longer than the retention:
//! archived in every bulk change tracking them, or detached from all of them
//! (`ChangesetStore::purgeable_before`). The merge requests stay on the code host. Unset
//! (the default), nothing is deleted.

use std::time::Duration;

use chrono::Utc;
use sourcegull_batches::store::ChangesetStore;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

/// Lease name of the retention janitor singleton.
pub const RETENTION_JANITOR: &str = "batches-changesets-retention";
/// How often the janitor runs.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Changesets deleted per statement.
const PURGE_BATCH: i64 = 500;
/// Counter of changesets deleted by the retention janitor.
pub const PURGED_METRIC: &str = metric_names::BATCHES_CHANGESETS_PURGED_TOTAL;

/// Deletes the changesets unused for longer than `retention`. Returns how many.
pub async fn purge_expired(db: &Db, retention: Duration) -> Result<u64> {
    let retention = chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::MAX);
    let Some(before) = Utc::now().checked_sub_signed(retention) else {
        return Ok(0);
    };
    let store = ChangesetStore::new(db);
    let mut total = 0;
    loop {
        let ids = store.purgeable_before(before, PURGE_BATCH).await?;
        if ids.is_empty() {
            break;
        }
        let n = store.delete(&ids).await?;
        total += n;
        if ids.len() < usize::try_from(PURGE_BATCH).unwrap_or(usize::MAX) || n == 0 {
            break;
        }
    }
    Ok(total)
}

/// Purges every [`RETENTION_INTERVAL`] (first right away) until `cancel` fires, reading the
/// setting each time.
pub async fn run_retention_janitor(db: Db, config: SiteConfigHandle, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(RETENTION_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let Some(retention) = config.load().value.batch_changes_changesets_retention else {
            continue;
        };
        match sourcegull_db::record_run(RETENTION_JANITOR, purge_expired(&db, retention.get()))
            .await
        {
            Ok(0) => {}
            Ok(n) => {
                metrics::counter!(PURGED_METRIC).increment(n);
                tracing::info!(
                    deleted = n,
                    "deleted changesets past batchChanges.changesetsRetention"
                );
            }
            Err(e) => tracing::warn!(error = %e, "purging archived changesets failed"),
        }
    }
}
