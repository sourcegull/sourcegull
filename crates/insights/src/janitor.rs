//! Retention and pruning: a leader-elected loop in the `worker` role that,
//! every hour, removes deleted series with everything they hold, keeps the newest
//! `insights.maximumSampleSize` points of every series (default 30), and deletes finished
//! jobs after a week.

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::DbError;
use tokio_util::sync::CancellationToken;

use crate::store::InsightStore;

/// Lease name of the janitor.
pub const JANITOR: &str = "insights-janitor";
/// How often the janitor runs.
pub const INTERVAL: Duration = Duration::from_secs(3600);
/// How long finished jobs are kept.
pub const JOB_RETENTION: TimeDelta = TimeDelta::days(7);

/// What one run removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JanitorReport {
    pub series: u64,
    pub points: u64,
    pub jobs: u64,
}

/// One janitor run, keeping `max_points` points per series.
pub async fn run_once(store: &InsightStore, max_points: i64) -> Result<JanitorReport, DbError> {
    let now = Utc::now();
    Ok(JanitorReport {
        series: store.purge_deleted(now).await?,
        points: store.prune_points(max_points.max(1)).await?,
        jobs: store.delete_finished_jobs(now - JOB_RETENTION).await?,
    })
}

/// Runs the janitor every [`INTERVAL`] (first run right away) until `cancel` fires.
pub async fn run_janitor(store: InsightStore, config: SiteConfigHandle, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let max = i64::from(config.load().value.insights_maximum_sample_size);
        match sourcegull_db::record_run(JANITOR, run_once(&store, max)).await {
            Ok(r) if r != JanitorReport::default() => tracing::info!(
                series = r.series,
                points = r.points,
                jobs = r.jobs,
                "insights pruned"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "insights janitor"),
        }
    }
}
