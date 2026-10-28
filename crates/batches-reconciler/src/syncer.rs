//! The changeset syncer: keeps the code host state of published changesets fresh between
//! reconciliations.
//!
//! [`run_syncer`] runs as a leader-elected singleton in the worker role. Every
//! [`SYNC_INTERVAL`] it loads the changesets due for a sync (never synced first) from the
//! code host through [`Reconciler::sync`]. The next sync is scheduled by [`next_sync`]: as
//! long after the last sync as the merge request had been quiet before it, between
//! [`MIN_SYNC_DELAY`] and [`MAX_SYNC_DELAY`], so active merge requests are synced every few
//! minutes and dormant ones a few times a day. Changesets the reconciler
//! is about to process (or processing) are skipped; it syncs them itself.
//!
//! Webhooks (`merge_request`, `pipeline`, [`crate::webhooks`]) update changesets
//! immediately, so changesets whose code host connection has webhooks configured are synced
//! less often: between [`WEBHOOK_MIN_SYNC_DELAY`] and [`WEBHOOK_MAX_SYNC_DELAY`], as a safety
//! net for lost deliveries.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_batches::store::{ChangesetState, ChangesetStore, ReconcilerState};
use sourcegull_db::Fence;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::error::{ReconcileError, Result};
use crate::executor::Reconciler;

/// Lease name of the changeset syncer singleton.
pub const SYNCER: &str = "batches-changeset-syncer";
/// How often the syncer looks for changesets due for a sync.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(30);
/// Changesets synced per pass.
pub const SYNC_BATCH: i64 = 50;
/// The shortest interval between syncs of a changeset.
pub const MIN_SYNC_DELAY: Duration = Duration::from_secs(2 * 60);
/// The longest interval between syncs of a changeset.
pub const MAX_SYNC_DELAY: Duration = Duration::from_secs(8 * 3600);
/// The shortest interval between syncs when the connection has webhooks.
pub const WEBHOOK_MIN_SYNC_DELAY: Duration = Duration::from_secs(3600);
/// The longest interval between syncs when the connection has webhooks.
pub const WEBHOOK_MAX_SYNC_DELAY: Duration = Duration::from_secs(24 * 3600);
/// When to try again after a failed sync.
pub const SYNC_ERROR_DELAY: Duration = Duration::from_secs(15 * 60);
/// Counter of changeset syncs, by `outcome` (`ok`, `error`).
pub const SYNCS_METRIC: &str = metric_names::BATCHES_CHANGESET_SYNCS_TOTAL;

fn chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

/// When to sync a changeset next after syncing it at `last_sync`, given the last change on
/// the code host (the merge request's update time or newest event) and whether its
/// connection sends webhooks. A change after the sync (a webhook arrived meanwhile) waits the
/// minimum delay after that change.
pub fn next_sync(
    last_sync: DateTime<Utc>,
    last_change: Option<DateTime<Utc>>,
    webhooks: bool,
) -> DateTime<Utc> {
    let (min, max) = if webhooks {
        (
            chrono(WEBHOOK_MIN_SYNC_DELAY),
            chrono(WEBHOOK_MAX_SYNC_DELAY),
        )
    } else {
        (chrono(MIN_SYNC_DELAY), chrono(MAX_SYNC_DELAY))
    };
    let Some(last_change) = last_change else {
        return last_sync + min;
    };
    let quiet = last_sync - last_change;
    if quiet < chrono::Duration::zero() {
        return last_change + min;
    }
    last_sync + quiet.clamp(min, max)
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub synced: usize,
    pub failed: usize,
    /// Left to the reconciler (queued or processing).
    pub skipped: usize,
}

/// Syncs up to `limit` changesets that are due, writing only while `fence` is current (a pass
/// of a syncer that lost its lease stops at its first write).
pub async fn sync_due(reconciler: &Reconciler, limit: i64, fence: &Fence) -> Result<SyncReport> {
    let store = ChangesetStore::new(reconciler.db());
    let ids = store.due_for_sync(limit).await?;
    let mut report = SyncReport::default();
    for ch in store.get_many(&ids).await? {
        if matches!(
            ch.reconciler_state,
            ReconcilerState::Queued | ReconcilerState::Processing | ReconcilerState::Errored
        ) || ch.state == ChangesetState::Processing
        {
            report.skipped += 1;
            continue;
        }
        match reconciler.sync(&ch, fence).await {
            Ok(()) => {
                report.synced += 1;
                metrics::counter!(SYNCS_METRIC, "outcome" => "ok").increment(1);
            }
            Err(ReconcileError::Db(e)) if e.is_fenced() => return Err(e.into()),
            Err(e) => {
                report.failed += 1;
                metrics::counter!(SYNCS_METRIC, "outcome" => "error").increment(1);
                tracing::info!(changeset = ch.id, error = %e, "syncing changeset failed");
                let retry = Utc::now() + chrono(SYNC_ERROR_DELAY);
                store
                    .set_syncer_error(ch.id, &e.to_string(), Some(retry), fence)
                    .await?;
            }
        }
    }
    Ok(report)
}

/// Runs sync passes, fenced by `fence`, every [`SYNC_INTERVAL`] until `cancel` fires.
pub async fn run_syncer(reconciler: Reconciler, fence: Fence, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(SYNC_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(SYNCER, sync_due(&reconciler, SYNC_BATCH, &fence)).await {
            Ok(r) if r.synced + r.failed > 0 => {
                tracing::debug!(
                    synced = r.synced,
                    failed = r.failed,
                    skipped = r.skipped,
                    "changeset sync pass"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "changeset sync pass failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_interval() {
        let now = Utc::now();
        let min = chrono(MIN_SYNC_DELAY);
        // Never changed: soon.
        assert_eq!(next_sync(now, None, false), now + min);
        // Changed just now: the minimum.
        assert_eq!(next_sync(now, Some(now), false), now + min);
        // Quiet for an hour: an hour.
        let hour = chrono::Duration::hours(1);
        assert_eq!(next_sync(now, Some(now - hour), false), now + hour);
        // Quiet for a week: the maximum.
        let week = chrono::Duration::days(7);
        assert_eq!(
            next_sync(now, Some(now - week), false),
            now + chrono(MAX_SYNC_DELAY)
        );
        // A change after the sync (webhook): the minimum after the change.
        let later = now + chrono::Duration::minutes(5);
        assert_eq!(next_sync(now, Some(later), false), later + min);

        // With webhooks: longer intervals.
        let wmin = chrono(WEBHOOK_MIN_SYNC_DELAY);
        assert_eq!(next_sync(now, Some(now), true), now + wmin);
        assert_eq!(next_sync(now, Some(later), true), later + wmin);
        let quiet = chrono::Duration::hours(5);
        assert_eq!(next_sync(now, Some(now - quiet), true), now + quiet);
        assert_eq!(
            next_sync(now, Some(now - week), true),
            now + chrono(WEBHOOK_MAX_SYNC_DELAY)
        );
    }
}
