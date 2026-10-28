//! The log janitor: deletes delivery logs older than `webhooks.outbound.logRetention`, and the
//! finished events they were for. A leader-elected singleton of the worker role.

use std::time::Duration;

use crate::store::OutboundWebhookStore;
use chrono::Utc;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

/// Lease name of the janitor.
pub const JANITOR: &str = "outbound-webhook-janitor";
/// How often the janitor runs.
pub const JANITOR_INTERVAL: Duration = Duration::from_secs(3600);
/// Counter of deleted delivery logs.
pub const LOGS_DELETED_METRIC: &str = metric_names::OUTBOUND_WEBHOOK_LOGS_DELETED_TOTAL;

/// Deletes logs and finished jobs older than the retention once. Returns the numbers deleted.
pub async fn clean(
    db: &Db,
    config: &SiteConfigHandle,
) -> Result<(u64, u64), sourcegull_db::DbError> {
    let retention = config.load().value.webhooks_outbound_log_retention.get();
    let retention = chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::MAX);
    let before = Utc::now()
        .checked_sub_signed(retention)
        .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC);
    let (logs, jobs) = OutboundWebhookStore::new(db, None)
        .delete_older_than(before)
        .await?;
    metrics::counter!(LOGS_DELETED_METRIC).increment(logs);
    Ok((logs, jobs))
}

/// Runs [`clean`] every [`JANITOR_INTERVAL`] (first run right away) until `cancel` fires.
pub async fn run_janitor(db: Db, config: SiteConfigHandle, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(JANITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(JANITOR, clean(&db, &config)).await {
            Ok((logs, jobs)) if logs + jobs > 0 => {
                tracing::info!(logs, jobs, "deleted old outbound webhook logs and events");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "deleting old outbound webhook logs failed"),
        }
    }
}
