//! The search jobs janitor, a leader-elected singleton in the worker role. Every
//! [`JANITOR_INTERVAL`] it deletes, with their results, the finished jobs created longer than
//! `searchJobs.retention` ago (when set), and removes result blobs whose job no longer
//! exists (a job deleted while one of its searches was writing).

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::Utc;
use sourcegull_blobstore::Blobs;
use sourcegull_config::SiteConfigHandle;
use tokio_util::sync::CancellationToken;

use crate::export::BLOB_ROOT;
use crate::store::SearchJobStore;

/// Lease name of the janitor singleton.
pub const JANITOR: &str = "search-jobs-janitor";
/// How often the janitor runs.
pub const JANITOR_INTERVAL: Duration = Duration::from_secs(3600);
/// Jobs deleted per pass at most.
const BATCH: i64 = 500;

/// What one pass deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JanitorReport {
    pub jobs: usize,
    pub orphan_blobs: usize,
}

/// One pass: expired jobs (with `retention`), then orphaned blobs.
pub async fn clean(
    store: &SearchJobStore,
    blobs: &Blobs,
    retention: Option<Duration>,
) -> Result<JanitorReport, crate::Error> {
    let mut report = JanitorReport::default();
    if let Some(retention) = retention {
        let retention = chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::MAX);
        if let Some(before) = Utc::now().checked_sub_signed(retention) {
            for id in store.expired(before, BATCH).await? {
                crate::delete_job(store, Some(blobs), id).await?;
                report.jobs += 1;
            }
        }
    }
    let keys = blobs.list(&format!("{BLOB_ROOT}/")).await?;
    let ids: BTreeSet<i64> = keys.iter().filter_map(|k| job_of(k)).collect();
    if !ids.is_empty() {
        let ids: Vec<i64> = ids.into_iter().collect();
        let live: BTreeSet<i64> = store.existing(&ids).await?.into_iter().collect();
        for key in &keys {
            if job_of(key).is_some_and(|id| !live.contains(&id)) {
                blobs.delete(key).await?;
                report.orphan_blobs += 1;
            }
        }
    }
    Ok(report)
}

/// The job id of a result blob key.
fn job_of(key: &str) -> Option<i64> {
    key.strip_prefix(BLOB_ROOT)?
        .strip_prefix('/')?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Runs [`clean`] every [`JANITOR_INTERVAL`] (first right away) until `cancel` fires, reading
/// the setting each time.
pub async fn run_janitor(
    store: SearchJobStore,
    blobs: Blobs,
    config: SiteConfigHandle,
    cancel: CancellationToken,
) {
    let mut tick = tokio::time::interval(JANITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let retention = config.load().value.search_jobs_retention.map(|d| d.get());
        match sourcegull_db::record_run(JANITOR, clean(&store, &blobs, retention)).await {
            Ok(r) if r == JanitorReport::default() => {}
            Ok(r) => tracing::info!(
                jobs = r.jobs,
                orphan_blobs = r.orphan_blobs,
                "deleted expired search jobs and orphaned results"
            ),
            Err(e) => tracing::warn!(error = %e, "cleaning up search jobs failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_of_keys() {
        assert_eq!(job_of("search-jobs/12/34.jsonl"), Some(12));
        assert_eq!(job_of("search-jobs/x/34.jsonl"), None);
        assert_eq!(job_of("uploads/12/34"), None);
    }
}
