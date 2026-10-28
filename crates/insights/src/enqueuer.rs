//! The enqueuer: a leader-elected loop in the `worker` role.
//!
//! Each pass:
//! - starts the backfill of new series: compiles the query (a query that cannot be counted
//!   fails the series with the reason), resolves the repositories, picks the backfill times
//!   (`insights.maximumSampleSize` points ending at the series' creation, one interval
//!   apart) and queues one job per repository, newer series first;
//! - completes backfills whose jobs have all finished (sums the aggregate);
//! - queues a recording for each completed series whose next point is due (the newest grid
//!   time after its last point; missed points are skipped, as their heads are gone).
//!
//! `NOTIFY insights` (a series created or deleted) wakes it; otherwise it polls.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{DbError, Fence};
use tokio_util::sync::CancellationToken;

use crate::jobs::Counter;
use crate::store::{BackfillState, CHANNEL};

/// Lease name of the enqueuer.
pub const ENQUEUER: &str = "insights-enqueuer";
/// How often the enqueuer runs without a notification.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassReport {
    pub backfills_started: u64,
    pub backfills_completed: u64,
    pub recordings_queued: u64,
    pub failed: u64,
}

/// The enqueuer's state.
pub struct Enqueuer {
    counter: Counter,
    config: Option<SiteConfigHandle>,
    /// Backfill and recording state is written only while this is current.
    fence: Fence,
}

impl Enqueuer {
    /// An enqueuer that is not fenced (see [`Enqueuer::with_fence`]).
    pub fn new(counter: Counter, config: Option<SiteConfigHandle>) -> Self {
        Self {
            counter,
            config,
            fence: Fence::none(ENQUEUER),
        }
    }

    /// Writes backfill and recording state only while `fence` is current, so an enqueuer
    /// that lost its lease does not overwrite its successor's.
    pub fn with_fence(mut self, fence: Fence) -> Self {
        self.fence = fence;
        self
    }

    fn max_points(&self) -> usize {
        self.config.as_ref().map_or(30, |c| {
            c.load().value.insights_maximum_sample_size.max(1) as usize
        })
    }

    /// One pass at `now` (see the module docs).
    pub async fn pass(&self, now: DateTime<Utc>) -> Result<PassReport, DbError> {
        let store = &self.counter.store;
        let mut report = PassReport::default();
        for series in store.list_live().await? {
            match series.backfill_state {
                BackfillState::New => {
                    let q = match Counter::compile(&series) {
                        Ok(q) => q,
                        Err(e) => {
                            store.set_failed(series.id, &e, &self.fence).await?;
                            report.failed += 1;
                            continue;
                        }
                    };
                    let repos = match self.counter.scope(&series, &q).await {
                        Ok(r) => r,
                        Err(sourcegull_pgqueue::JobError::Fatal(e)) => {
                            store.set_failed(series.id, &e, &self.fence).await?;
                            report.failed += 1;
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(series = series.id, error = %e, "insights: repositories not resolved");
                            continue;
                        }
                    };
                    let times = series
                        .interval
                        .backfill_times(series.anchor(), self.max_points());
                    let ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
                    store
                        .start_backfill(series.id, &times, &ids, series.priority(), &self.fence)
                        .await?;
                    report.backfills_started += 1;
                }
                BackfillState::Completed => {
                    let last = series.last_recorded_at.unwrap_or(series.anchor());
                    let Some(due) = series.interval.due(series.anchor(), last, now) else {
                        continue;
                    };
                    if store
                        .pending_recording(series.id)
                        .await?
                        .is_some_and(|p| p >= due)
                    {
                        continue;
                    }
                    if store.enqueue_recording(series.id, due, &self.fence).await? {
                        report.recordings_queued += 1;
                    }
                }
                BackfillState::Running | BackfillState::Failed => {}
            }
        }
        for id in store.backfills_done().await? {
            if let Some(series) = store.get(id).await? {
                store.finish_backfill(&series, &self.fence).await?;
                report.backfills_completed += 1;
            }
        }
        Ok(report)
    }
}

/// Runs the enqueuer until `cancel` fires.
pub async fn run_enqueuer(enqueuer: Enqueuer, cancel: CancellationToken) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
    let listener = tokio::spawn(listen(
        enqueuer.counter.backend.db.clone(),
        tx,
        cancel.clone(),
    ));
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            w = rx.recv() => if w.is_none() { break },
            _ = poll.tick() => {}
        }
        match sourcegull_db::record_run(ENQUEUER, enqueuer.pass(Utc::now())).await {
            Ok(r) if r != PassReport::default() => tracing::info!(
                started = r.backfills_started,
                completed = r.backfills_completed,
                recordings = r.recordings_queued,
                failed = r.failed,
                "insights enqueued"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "insights enqueuer pass"),
        }
    }
    listener.abort();
}

async fn listen(
    db: sourcegull_db::Db,
    tx: tokio::sync::mpsc::Sender<()>,
    cancel: CancellationToken,
) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(db.pool(), &[CHANNEL]) => s,
    };
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            // A full queue already holds a wake-up.
            _ = sub.recv() => { let _ = tx.try_send(()); }
        }
    }
}
