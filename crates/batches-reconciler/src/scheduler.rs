//! The rollout window scheduler.
//!
//! With `batchChanges.rolloutWindows` configured, the reconciler moves a changeset whose plan
//! publishes it to `scheduled` instead of running the plan ([`Reconciler::reconcile`]). The
//! [`Scheduler`], a leader-elected singleton in the worker role ([`run_scheduler`]), moves
//! scheduled changesets back to `queued` (marked `window_released`, so the reconciler then
//! publishes them) in order of their last update, at the rate of the window in effect:
//!
//! - `unlimited`: all of them, in batches of [`RELEASE_BATCH`];
//! - `<n>/<unit>`: through a [`TokenBucket`] of one token per `unit / n` with a burst of
//!   [`BURST`] (evenly spaced). The bucket starts full when a
//!   window starts and carries over while the same window stays in effect (across
//!   midnight);
//! - `0`, or outside every window: none.
//!
//! Without windows every scheduled changeset is released (windows were removed). The
//! scheduler wakes when the next token is due, when the window changes, and at least every
//! [`SCHEDULER_POLL`] to see newly scheduled changesets. Time comes from a [`Clock`], so tests
//! drive it with a [`FakeClock`]. `ChangesetStore::release_now` releases chosen changesets
//! regardless of windows ("publish now").
//!
//! [`Reconciler::reconcile`]: crate::Reconciler::reconcile

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_batches::store::ChangesetStore;
use sourcegull_config::{RolloutRate, RolloutWindows, SiteConfigHandle};
use sourcegull_db::{Db, Fence};
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::error::Result;

/// Lease name of the scheduler singleton.
pub const SCHEDULER: &str = "batches-changeset-scheduler";
/// The longest the scheduler sleeps (to see newly scheduled changesets and config changes).
pub const SCHEDULER_POLL: Duration = Duration::from_secs(10);
/// Changesets released per statement when the rate is unlimited.
pub const RELEASE_BATCH: i64 = 100;
/// Tokens a limited window's bucket holds at most.
pub const BURST: u32 = 1;
/// Counter of changesets released from `scheduled`.
pub const RELEASED_METRIC: &str = metric_names::BATCHES_CHANGESETS_RELEASED_TOTAL;

/// The current time.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock tests set and advance.
#[derive(Debug, Clone)]
pub struct FakeClock(Arc<Mutex<DateTime<Utc>>>);

impl FakeClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = now;
    }

    pub fn advance(&self, by: chrono::Duration) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A token bucket refilled with one token per `interval`, holding up to `burst` tokens
/// (kept as the theoretical arrival time of the next token, GCRA style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBucket {
    interval: chrono::Duration,
    burst: u32,
    /// When the bucket is next empty-and-refilled by one token.
    tat: DateTime<Utc>,
}

impl TokenBucket {
    /// A full bucket at `now`.
    pub fn new(interval: chrono::Duration, burst: u32, now: DateTime<Utc>) -> Self {
        Self {
            interval,
            burst: burst.max(1),
            tat: now,
        }
    }

    /// When the next token is available.
    pub fn next_available(&self) -> DateTime<Utc> {
        self.tat - self.interval * i32::try_from(self.burst - 1).unwrap_or(i32::MAX)
    }

    /// Takes a token if one is available at `now`.
    pub fn try_take(&mut self, now: DateTime<Utc>) -> bool {
        if now < self.next_available() {
            return false;
        }
        self.tat = self.tat.max(now) + self.interval;
        true
    }
}

/// What one [`Scheduler::tick`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// The changesets released to the reconciler.
    pub released: Vec<i64>,
    /// When to tick again.
    pub next: DateTime<Utc>,
}

/// Releases scheduled changesets at the rollout windows' rates.
pub struct Scheduler {
    db: Db,
    config: SiteConfigHandle,
    clock: Arc<dyn Clock>,
    /// The bucket of the limited window in effect (by index and rate).
    bucket: Option<(usize, RolloutRate, TokenBucket)>,
    /// Changesets are released only while this is current.
    fence: Fence,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

fn chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

impl Scheduler {
    /// A scheduler that is not fenced (see [`Scheduler::with_fence`]).
    pub fn new(db: Db, config: SiteConfigHandle, clock: Arc<dyn Clock>) -> Self {
        Self {
            db,
            config,
            clock,
            bucket: None,
            fence: Fence::none(SCHEDULER),
        }
    }

    /// Releases changesets only while `fence` is current, so a scheduler that lost its lease
    /// does not release beyond the window's rate next to its successor.
    pub fn with_fence(mut self, fence: Fence) -> Self {
        self.fence = fence;
        self
    }

    /// Releases what the window in effect allows now.
    pub async fn tick(&mut self) -> Result<Tick> {
        let now = self.clock.now();
        let poll = now + chrono(SCHEDULER_POLL);
        let windows =
            RolloutWindows::lenient(&self.config.load().value.batch_changes_rollout_windows);
        let store = ChangesetStore::new(&self.db);
        let (released, next) = if windows.is_empty() {
            self.bucket = None;
            let released = store.release_scheduled(RELEASE_BATCH, &self.fence).await?;
            let more = released.len() >= usize::try_from(RELEASE_BATCH).unwrap_or(usize::MAX);
            (released, if more { now } else { poll })
        } else {
            let seg = windows.at(now);
            let until = seg.until.min(poll);
            match (seg.rate, seg.window) {
                (Some(RolloutRate::Unlimited), _) => {
                    self.bucket = None;
                    let released = store.release_scheduled(RELEASE_BATCH, &self.fence).await?;
                    let more =
                        released.len() >= usize::try_from(RELEASE_BATCH).unwrap_or(usize::MAX);
                    (released, if more { now } else { until })
                }
                (Some(rate @ RolloutRate::Limited { .. }), Some(w)) => {
                    let interval = rate.interval().map_or(chrono::Duration::MAX, chrono);
                    let bucket = match &mut self.bucket {
                        Some((bw, br, b)) if *bw == w && *br == rate => b,
                        slot => {
                            &mut slot
                                .insert((w, rate, TokenBucket::new(interval, BURST, now)))
                                .2
                        }
                    };
                    let mut released = Vec::new();
                    while bucket.next_available() <= now {
                        let ids = store.release_scheduled(1, &self.fence).await?;
                        if ids.is_empty() {
                            break;
                        }
                        bucket.try_take(now);
                        released.extend(ids);
                    }
                    let next = if bucket.next_available() <= now {
                        until
                    } else {
                        bucket.next_available().min(until)
                    };
                    (released, next)
                }
                _ => {
                    self.bucket = None;
                    (Vec::new(), until)
                }
            }
        };
        if !released.is_empty() {
            metrics::counter!(RELEASED_METRIC).increment(released.len() as u64);
            tracing::info!(released = released.len(), "released scheduled changesets");
        }
        Ok(Tick { released, next })
    }
}

/// Runs [`Scheduler::tick`] whenever it asks to until `cancel` fires.
pub async fn run_scheduler(mut scheduler: Scheduler, cancel: CancellationToken) {
    loop {
        let wait = match sourcegull_db::record_run(SCHEDULER, scheduler.tick()).await {
            Ok(tick) => (tick.next - scheduler.clock.now())
                .to_std()
                .unwrap_or(Duration::ZERO)
                .min(SCHEDULER_POLL),
            Err(e) => {
                tracing::warn!(error = %e, "changeset scheduler pass failed");
                SCHEDULER_POLL
            }
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket() {
        let t0 = Utc::now();
        let six = chrono::Duration::minutes(6);
        let mut b = TokenBucket::new(six, 1, t0);
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        assert_eq!(b.next_available(), t0 + six);
        assert!(!b.try_take(t0 + chrono::Duration::minutes(5)));
        assert!(b.try_take(t0 + six));
        // Idle for an hour: still only one token (burst 1).
        let later = t0 + chrono::Duration::hours(1);
        assert!(b.try_take(later));
        assert!(!b.try_take(later));

        // A burst of 3 refills one per interval.
        let mut b = TokenBucket::new(six, 3, t0);
        assert!(b.try_take(t0) && b.try_take(t0) && b.try_take(t0));
        assert!(!b.try_take(t0));
        assert_eq!(b.next_available(), t0 + six);
        assert!(b.try_take(t0 + six));
        assert!(!b.try_take(t0 + six));
    }
}
