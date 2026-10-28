//! The backfiller and the recorder, pgqueue workers in the `worker` role.
//!
//! - **Backfiller** (`insight_backfill_jobs`, one job per series and repository, newest
//!   series first): asks the repository's node to count the series at every backfill time
//!   (`CountSeries`: in full at the oldest point, then through tree diffs), stores the points
//!   and the state the recorder continues from. Each job stands alone, so a backfill resumes
//!   where it stopped; the enqueuer completes the series when no job is left.
//! - **Recorder** (`insight_recording_jobs`, one job per series and point time): repositories
//!   whose refs did not change since their last point get that value again without any
//!   search; the others are counted at their head from their last point (the same delta
//!   technique). Repositories new to the scope are counted in full.
//!
//! Node calls go through one [`RateLimiter`] per worker (`insights.query.worker.rateLimit`,
//! default 20 per second); `insights.query.worker.concurrency` backfill jobs run at once
//! (default 1).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sourcegull_config::SiteConfigHandle;
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search_exec::series::{counts_from_pb, counts_to_pb};
use sourcegull_search_web::SearchBackend;
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;
use tokio::time::Instant;

use crate::compile::{CompiledQuery, compile};
use crate::scope::{ScopedRepo, resolve};
use crate::store::{
    BackfillJob, BackfillState, InsightStore, RecordingJob, RepoPoint, RepoState, Series,
};

/// Counter of node calls, by `job` (`backfill`, `record`).
pub const COUNT_CALLS_METRIC: &str = metric_names::INSIGHTS_COUNT_CALLS_TOTAL;
/// Counter of repositories whose value was carried forward without a search.
pub const CARRIED_METRIC: &str = metric_names::INSIGHTS_CARRIED_FORWARD_TOTAL;

/// Repositories written per transaction while recording.
const RECORD_BATCH: usize = 200;

/// A token bucket over node calls, its rate read live from the site configuration.
pub struct RateLimiter {
    config: Option<SiteConfigHandle>,
    state: tokio::sync::Mutex<(f64, Instant)>,
}

impl RateLimiter {
    /// A limiter following `insights.query.worker.rateLimit`; `None` does not limit.
    pub fn new(config: Option<SiteConfigHandle>) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new((1.0, Instant::now())),
        }
    }

    /// Waits for a token.
    pub async fn acquire(&self) {
        let Some(config) = &self.config else {
            return;
        };
        loop {
            let rate = config
                .load()
                .value
                .insights_query_worker_rate_limit
                .max(0.01);
            let wait = {
                let mut s = self.state.lock().await;
                let now = Instant::now();
                let burst = rate.max(1.0);
                s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * rate).min(burst);
                s.1 = now;
                if s.0 >= 1.0 {
                    s.0 -= 1.0;
                    return;
                }
                (1.0 - s.0) / rate
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

fn secs(t: DateTime<Utc>) -> i64 {
    t.timestamp()
}

pub(crate) fn time(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().unwrap_or_default()
}

/// The `CountSeries` request counting `q` in `repo` at `times` (at the head of its revision
/// with `at_head`), continuing from `base` when given.
pub fn count_request(
    repo: &ScopedRepo,
    q: &CompiledQuery,
    times: &[DateTime<Utc>],
    at_head: bool,
    base: Option<&RepoState>,
) -> pb::CountSeriesRequest {
    pb::CountSeriesRequest {
        repo: Some(pb::RepoRef {
            id: repo.id,
            name: repo.name.clone(),
        }),
        rev: q.rev.clone(),
        spec: Some(q.spec.to_pb()),
        times: times.iter().copied().map(secs).collect(),
        at_head,
        base: base
            .filter(|b| !b.commit.is_empty())
            .map(|b| pb::SeriesBase {
                commit_oid: b.commit.clone(),
                counts: counts_to_pb(&b.counts),
            }),
        full: false,
    }
}

/// What the jobs share.
#[derive(Clone)]
pub struct Counter {
    pub backend: SearchBackend,
    pub store: InsightStore,
    pub limiter: Arc<RateLimiter>,
}

impl Counter {
    pub fn new(backend: SearchBackend, config: Option<SiteConfigHandle>) -> Self {
        let store = InsightStore::new(&backend.db);
        Self {
            backend,
            store,
            limiter: Arc::new(RateLimiter::new(config)),
        }
    }

    /// Compiles a series' query as it was when the series was created.
    pub fn compile(series: &Series) -> Result<CompiledQuery, String> {
        compile(
            &series.query,
            series.method,
            series.group_by,
            series.created_at,
        )
    }

    /// The repositories `series` counts now.
    pub async fn scope(
        &self,
        series: &Series,
        q: &CompiledQuery,
    ) -> Result<Vec<ScopedRepo>, JobError> {
        let catalog = self
            .backend
            .catalog
            .get(&self.backend.db)
            .await
            .map_err(JobError::retry)?;
        let repos = resolve(&catalog, &series.scope, q).map_err(JobError::fatal)?;
        let ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
        let not_cloned = self.store.not_cloned(&ids).await.map_err(JobError::retry)?;
        Ok(repos
            .into_iter()
            .filter(|r| !not_cloned.contains(&r.id))
            .collect())
    }

    /// Calls `CountSeries` on the repository's node.
    async fn count(
        &self,
        repo: &ScopedRepo,
        q: &CompiledQuery,
        times: &[DateTime<Utc>],
        at_head: bool,
        base: Option<&RepoState>,
        job: &'static str,
    ) -> Result<pb::CountSeriesResponse, JobError> {
        self.limiter.acquire().await;
        metrics::counter!(COUNT_CALLS_METRIC, "job" => job).increment(1);
        let mut client = self
            .backend
            .repo_clients
            .search_for_repo(RepoId(repo.id))
            .map_err(JobError::retry)?;
        let req = count_request(repo, q, times, at_head, base);
        let resp = client
            .count_series(req)
            .await
            .map_err(|s| JobError::retry(format!("counting {}: {}", repo.name, s.message())))?
            .into_inner();
        if let Some(s) = &resp.stats {
            tracing::debug!(
                repo = %repo.name,
                full = s.full_points,
                delta = s.delta_points,
                same = s.same_points,
                files = s.files_counted,
                cache_hits = s.cache_hits,
                elapsed_ms = s.elapsed_ms,
                "insight series counted"
            );
        }
        Ok(resp)
    }

    /// Backfills one series in one repository (the backfill job's body).
    pub async fn backfill(&self, job: &BackfillJob) -> Result<(), JobError> {
        let Some(series) = self
            .store
            .get(job.series_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(());
        };
        if series.backfill_state != BackfillState::Running {
            return Ok(());
        }
        let q = Self::compile(&series).map_err(JobError::fatal)?;
        let catalog = self
            .backend
            .catalog
            .get(&self.backend.db)
            .await
            .map_err(JobError::retry)?;
        // Deleted or blocked since the backfill was queued: nothing to count.
        let Some(repo) = catalog.get(job.repo_id) else {
            return Ok(());
        };
        let repo = ScopedRepo {
            id: repo.id,
            name: repo.name.clone(),
        };
        let resp = self
            .count(&repo, &q, &series.backfill_times, false, None, "backfill")
            .await?;
        let points: Vec<RepoPoint> = resp
            .points
            .iter()
            .map(|p| RepoPoint {
                repo_id: repo.id,
                time: time(p.time),
                counts: counts_from_pb(&p.counts),
            })
            .collect();
        let state = resp.points.last().map(|p| RepoState {
            repo_id: repo.id,
            time: time(p.time),
            commit: p.commit_oid.clone(),
            counts: counts_from_pb(&p.counts),
            // The last point is the commit at the backfill's newest time: refs changed
            // after that time must be counted again by the next recording.
            checked_at: time(p.time),
        });
        self.store
            .write_points(series.id, &points, state.as_slice())
            .await
            .map_err(JobError::retry)
    }

    /// Records one series at `time` (the recording job's body).
    pub async fn record(&self, job: &RecordingJob) -> Result<RecordReport, JobError> {
        let mut report = RecordReport::default();
        let Some(series) = self
            .store
            .get(job.series_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(report);
        };
        if series.backfill_state != BackfillState::Completed {
            return Ok(report);
        }
        let q = Self::compile(&series).map_err(JobError::fatal)?;
        let repos = self.scope(&series, &q).await?;
        let states: HashMap<i32, RepoState> = self
            .store
            .repo_states(series.id)
            .await
            .map_err(JobError::retry)?
            .into_iter()
            .map(|s| (s.repo_id, s))
            .collect();
        let ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
        let changed = self
            .store
            .repos_last_changed(&ids)
            .await
            .map_err(JobError::retry)?;
        let (mut points, mut new_states) = (Vec::new(), Vec::new());
        for repo in &repos {
            let state = states.get(&repo.id);
            // Already recorded by an earlier attempt of this job.
            if state.is_some_and(|s| s.time >= job.time) {
                continue;
            }
            let unchanged =
                state.is_some_and(|s| changed.get(&repo.id).is_none_or(|at| *at < s.checked_at));
            let next = match state {
                Some(s) if unchanged => {
                    report.carried += 1;
                    RepoState {
                        time: job.time,
                        ..s.clone()
                    }
                }
                _ => {
                    report.counted += 1;
                    let checked_at = Utc::now();
                    let resp = self
                        .count(repo, &q, &[job.time], true, state, "record")
                        .await?;
                    let p = resp.points.into_iter().next().unwrap_or_default();
                    RepoState {
                        repo_id: repo.id,
                        time: job.time,
                        commit: p.commit_oid,
                        counts: counts_from_pb(&p.counts),
                        checked_at,
                    }
                }
            };
            points.push(RepoPoint {
                repo_id: repo.id,
                time: job.time,
                counts: next.counts.clone(),
            });
            new_states.push(next);
            if points.len() >= RECORD_BATCH {
                self.store
                    .write_points(series.id, &points, &new_states)
                    .await
                    .map_err(JobError::retry)?;
                points.clear();
                new_states.clear();
            }
        }
        self.store
            .write_points(series.id, &points, &new_states)
            .await
            .map_err(JobError::retry)?;
        self.store
            .finish_recording(series.id, job.time)
            .await
            .map_err(JobError::retry)?;
        metrics::counter!(CARRIED_METRIC).increment(report.carried);
        Ok(report)
    }
}

/// What a recording did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordReport {
    /// Repositories counted on their node.
    pub counted: u64,
    /// Repositories whose value was carried forward without a search.
    pub carried: u64,
}

struct BackfillHandler(Counter);

#[async_trait]
impl Handler<BackfillJob> for BackfillHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<BackfillJob>) -> Result<(), JobError> {
        self.0.backfill(&record.payload).await
    }
}

struct RecordingHandler(Counter);

#[async_trait]
impl Handler<RecordingJob> for RecordingHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<RecordingJob>) -> Result<(), JobError> {
        let r = self.0.record(&record.payload).await?;
        tracing::info!(
            series = record.payload.series_id,
            time = %record.payload.time,
            counted = r.counted,
            carried = r.carried,
            "insight series recorded"
        );
        Ok(())
    }
}

/// The backfill and recording workers with their stalled-job resetters, ready to run.
pub struct Workers {
    pub backfill: Worker<BackfillJob>,
    pub backfill_resetter: Resetter<BackfillJob>,
    pub recording: Worker<RecordingJob>,
    pub recording_resetter: Resetter<RecordingJob>,
}

impl Workers {
    /// Runs all four until `cancel` fires.
    pub async fn run(self, cancel: tokio_util::sync::CancellationToken) {
        tokio::join!(
            self.backfill.run(cancel.clone()),
            self.backfill_resetter.run(cancel.clone()),
            self.recording.run(cancel.clone()),
            self.recording_resetter.run(cancel),
        );
    }
}

/// The trend workers; `concurrency` backfill jobs at once.
pub fn workers(
    counter: Counter,
    worker_id: &str,
    concurrency: usize,
) -> Result<Workers, sourcegull_pgqueue::Error> {
    let pool = counter.store.pool().clone();
    let backfill: Queue<BackfillJob> =
        Queue::new(pool.clone(), InsightStore::backfill_queue_config())?;
    let recording: Queue<RecordingJob> = Queue::new(pool, InsightStore::recording_queue_config())?;
    let mut opts = WorkerOptions::new(format!("{worker_id}-insights-backfill"));
    opts.concurrency = concurrency.max(1);
    let backfill_worker = Worker::new(backfill.clone(), BackfillHandler(counter.clone()), opts);
    let opts = WorkerOptions::new(format!("{worker_id}-insights-recording"));
    let recording_worker = Worker::new(recording.clone(), RecordingHandler(counter), opts);
    Ok(Workers {
        backfill: backfill_worker,
        backfill_resetter: Resetter::new(backfill, Duration::from_secs(30)),
        recording: recording_worker,
        recording_resetter: Resetter::new(recording, Duration::from_secs(30)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_limiter_spaces_calls_at_the_configured_rate() {
        let config = sourcegull_config::ConfigHandle::new(
            1,
            sourcegull_config::SiteConfig {
                insights_query_worker_rate_limit: 2.0,
                ..Default::default()
            },
        );
        let limiter = RateLimiter::new(Some(config));
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        // The first call at once, then one every 500 ms.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(1900), "{elapsed:?}");
        assert!(elapsed <= Duration::from_millis(2200), "{elapsed:?}");
    }
}
