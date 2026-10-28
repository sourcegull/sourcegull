//! Background routine runs: which process holds each leader-elected routine and
//! how its recent runs went, for the site admin "Background jobs" page.
//!
//! A *routine* is a named singleton loop (its lease name, e.g. `repo-sync-scheduler`). Its
//! *instances* are the processes that held its lease ([`RoutineRecorder::started`] /
//! [`RoutineRecorder::stopped`], reported by the lease runner), and a *run* is one pass of its
//! loop ([`record_run`]). Recorders are bounded: the newest [`RECENT_RUNS_KEPT`] runs per
//! routine and daily statistics for [`STATS_DAYS`] days.
//!
//! As with every shared state, there are two implementations of [`RoutineRecorder`]:
//! [`MemoryRoutineRecorder`] (one process, the default) and [`PgRoutineRecorder`] (shared by
//! every process; startup installs it with [`set_global_recorder`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Display;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::Result;

/// Runs kept per routine.
pub const RECENT_RUNS_KEPT: usize = 50;
/// Days of run statistics kept (today included).
pub const STATS_DAYS: i64 = 7;
/// Instances stopped longer ago than this many days are forgotten.
pub const INSTANCE_RETENTION_DAYS: i64 = 7;

/// One process that held a routine's lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineInstance {
    /// The process' host name.
    pub host_name: String,
    /// When it last took the lease.
    pub last_started_at: Option<DateTime<Utc>>,
    /// When it last gave the lease up.
    pub last_stopped_at: Option<DateTime<Utc>>,
}

impl RoutineInstance {
    /// Whether the instance holds the lease now (started after it last stopped).
    pub fn is_running(&self) -> bool {
        match (self.last_started_at, self.last_stopped_at) {
            (Some(start), Some(stop)) => start > stop,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// One pass of a routine's loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineRun {
    /// When the run started.
    pub at: DateTime<Utc>,
    /// The host that ran it.
    pub host_name: String,
    /// How long it took.
    pub duration_ms: i64,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

/// Run statistics over the last [`STATS_DAYS`] days.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineStats {
    /// The start of the earliest day with runs.
    pub since: Option<DateTime<Utc>>,
    pub run_count: i64,
    pub error_count: i64,
    pub min_duration_ms: i64,
    pub avg_duration_ms: i64,
    pub max_duration_ms: i64,
}

/// Everything recorded about one routine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineSnapshot {
    /// The routine (lease) name.
    pub name: String,
    /// Processes that held its lease, by host name.
    pub instances: Vec<RoutineInstance>,
    /// The newest runs, newest first.
    pub recent_runs: Vec<RoutineRun>,
    pub stats: RoutineStats,
}

/// Records routine instances and runs (in-process or Postgres).
#[async_trait]
pub trait RoutineRecorder: Send + Sync + std::fmt::Debug {
    /// `host` took the lease of `routine`.
    async fn started(&self, routine: &str, host: &str) -> Result<()>;
    /// `host` gave up the lease of `routine`.
    async fn stopped(&self, routine: &str, host: &str) -> Result<()>;
    /// `routine` finished a run.
    async fn record_run(&self, routine: &str, run: &RoutineRun) -> Result<()>;
    /// Every routine recorded, by name, with up to `recent_runs` runs each.
    async fn snapshot(&self, recent_runs: usize) -> Result<Vec<RoutineSnapshot>>;
}

fn global_slot() -> &'static RwLock<Arc<dyn RoutineRecorder>> {
    static GLOBAL: OnceLock<RwLock<Arc<dyn RoutineRecorder>>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(Arc::new(MemoryRoutineRecorder::default())))
}

/// The process-wide recorder: in-process unless [`set_global_recorder`] replaced it.
pub fn global_recorder() -> Arc<dyn RoutineRecorder> {
    global_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Replaces the process-wide recorder (at startup).
pub fn set_global_recorder(recorder: Arc<dyn RoutineRecorder>) {
    *global_slot().write().unwrap_or_else(|e| e.into_inner()) = recorder;
}

/// This process' host name as routines report it (`HOSTNAME`, else `localhost`).
pub fn routine_host_name() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "localhost".to_owned())
    })
}

/// Runs one pass of `routine` and records it with the global recorder (best effort: a
/// failure to record is ignored). Returns the pass' result.
pub async fn record_run<T, E, F>(routine: &str, pass: F) -> std::result::Result<T, E>
where
    E: Display,
    F: Future<Output = std::result::Result<T, E>>,
{
    let at = Utc::now();
    let start = std::time::Instant::now();
    let out = pass.await;
    let run = RoutineRun {
        at,
        host_name: routine_host_name().to_owned(),
        duration_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
        error: out.as_ref().err().map(ToString::to_string),
    };
    let _ = global_recorder().record_run(routine, &run).await;
    out
}

#[derive(Debug, Clone, Copy)]
struct DayStats {
    runs: i64,
    errors: i64,
    min: i64,
    max: i64,
    total: i64,
}

#[derive(Debug, Default)]
struct MemoryRoutine {
    instances: BTreeMap<String, RoutineInstance>,
    runs: VecDeque<RoutineRun>,
    days: BTreeMap<NaiveDate, DayStats>,
}

/// In-process recorder (one process only).
#[derive(Debug, Default)]
pub struct MemoryRoutineRecorder {
    routines: Mutex<HashMap<String, MemoryRoutine>>,
}

impl MemoryRoutineRecorder {
    fn with<R>(&self, routine: &str, f: impl FnOnce(&mut MemoryRoutine) -> R) -> R {
        let mut map = self.routines.lock().unwrap_or_else(|e| e.into_inner());
        f(map.entry(routine.to_owned()).or_default())
    }
}

fn day_start(day: NaiveDate) -> Option<DateTime<Utc>> {
    day.and_hms_opt(0, 0, 0).map(|t| t.and_utc())
}

fn stats_from(days: impl Iterator<Item = (NaiveDate, DayStats)>) -> RoutineStats {
    let mut out = RoutineStats::default();
    let mut total = 0;
    for (day, d) in days {
        if out.since.is_none() {
            out.since = day_start(day);
        }
        out.min_duration_ms = if out.run_count == 0 {
            d.min
        } else {
            out.min_duration_ms.min(d.min)
        };
        out.max_duration_ms = out.max_duration_ms.max(d.max);
        out.run_count += d.runs;
        out.error_count += d.errors;
        total += d.total;
    }
    if out.run_count > 0 {
        out.avg_duration_ms = total / out.run_count;
    }
    out
}

#[async_trait]
impl RoutineRecorder for MemoryRoutineRecorder {
    async fn started(&self, routine: &str, host: &str) -> Result<()> {
        self.with(routine, |r| {
            r.instances
                .entry(host.to_owned())
                .or_insert_with(|| RoutineInstance {
                    host_name: host.to_owned(),
                    last_started_at: None,
                    last_stopped_at: None,
                })
                .last_started_at = Some(Utc::now());
        });
        Ok(())
    }

    async fn stopped(&self, routine: &str, host: &str) -> Result<()> {
        self.with(routine, |r| {
            r.instances
                .entry(host.to_owned())
                .or_insert_with(|| RoutineInstance {
                    host_name: host.to_owned(),
                    last_started_at: None,
                    last_stopped_at: None,
                })
                .last_stopped_at = Some(Utc::now());
        });
        Ok(())
    }

    async fn record_run(&self, routine: &str, run: &RoutineRun) -> Result<()> {
        self.with(routine, |r| {
            r.runs.push_front(run.clone());
            r.runs.truncate(RECENT_RUNS_KEPT);
            let day = run.at.date_naive();
            let e = i64::from(run.error.is_some());
            r.days
                .entry(day)
                .and_modify(|d| {
                    d.runs += 1;
                    d.errors += e;
                    d.min = d.min.min(run.duration_ms);
                    d.max = d.max.max(run.duration_ms);
                    d.total += run.duration_ms;
                })
                .or_insert(DayStats {
                    runs: 1,
                    errors: e,
                    min: run.duration_ms,
                    max: run.duration_ms,
                    total: run.duration_ms,
                });
            let oldest = Utc::now().date_naive() - chrono::Duration::days(STATS_DAYS - 1);
            r.days.retain(|d, _| *d >= oldest);
        });
        Ok(())
    }

    async fn snapshot(&self, recent_runs: usize) -> Result<Vec<RoutineSnapshot>> {
        let map = self.routines.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = Utc::now().date_naive() - chrono::Duration::days(STATS_DAYS - 1);
        let mut out: Vec<RoutineSnapshot> = map
            .iter()
            .map(|(name, r)| RoutineSnapshot {
                name: name.clone(),
                instances: r.instances.values().cloned().collect(),
                recent_runs: r.runs.iter().take(recent_runs).cloned().collect(),
                stats: stats_from(
                    r.days
                        .iter()
                        .filter(|(d, _)| **d >= oldest)
                        .map(|(d, s)| (*d, *s)),
                ),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Postgres recorder (`background_routine_*` tables), shared by every process.
#[derive(Debug, Clone)]
pub struct PgRoutineRecorder {
    pool: PgPool,
}

impl PgRoutineRecorder {
    /// A recorder on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RoutineRecorder for PgRoutineRecorder {
    async fn started(&self, routine: &str, host: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO background_routine_instances (routine, host_name, last_started_at)
             VALUES ($1, $2, now())
             ON CONFLICT (routine, host_name) DO UPDATE SET last_started_at = now()",
        )
        .bind(routine)
        .bind(host)
        .execute(&self.pool)
        .await?;
        // Hosts come and go (pod names): forget long-stopped ones.
        sqlx::query(
            "DELETE FROM background_routine_instances
             WHERE last_stopped_at > coalesce(last_started_at, '-infinity')
               AND last_stopped_at < now() - make_interval(days => $1)",
        )
        .bind(i32::try_from(INSTANCE_RETENTION_DAYS).unwrap_or(7))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn stopped(&self, routine: &str, host: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO background_routine_instances (routine, host_name, last_stopped_at)
             VALUES ($1, $2, now())
             ON CONFLICT (routine, host_name) DO UPDATE SET last_stopped_at = now()",
        )
        .bind(routine)
        .bind(host)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_run(&self, routine: &str, run: &RoutineRun) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO background_routine_runs (routine, host_name, at, duration_ms, error)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(routine)
        .bind(&run.host_name)
        .bind(run.at)
        .bind(run.duration_ms.max(0))
        .bind(&run.error)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO background_routine_stats AS s
                 (routine, day, run_count, error_count, min_duration_ms, max_duration_ms,
                  total_duration_ms)
             VALUES ($1, ($2 AT TIME ZONE 'UTC')::date, 1, $3, $4, $4, $4)
             ON CONFLICT (routine, day) DO UPDATE SET
                 run_count = s.run_count + 1,
                 error_count = s.error_count + EXCLUDED.error_count,
                 min_duration_ms = least(s.min_duration_ms, EXCLUDED.min_duration_ms),
                 max_duration_ms = greatest(s.max_duration_ms, EXCLUDED.max_duration_ms),
                 total_duration_ms = s.total_duration_ms + EXCLUDED.total_duration_ms",
        )
        .bind(routine)
        .bind(run.at)
        .bind(i64::from(run.error.is_some()))
        .bind(run.duration_ms.max(0))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM background_routine_runs WHERE routine = $1 AND id <= (
                 SELECT id FROM background_routine_runs WHERE routine = $1
                 ORDER BY id DESC OFFSET $2 LIMIT 1)",
        )
        .bind(routine)
        .bind(i64::try_from(RECENT_RUNS_KEPT).unwrap_or(50))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM background_routine_stats
             WHERE day < (now() AT TIME ZONE 'UTC')::date - ($1::int - 1)",
        )
        .bind(i32::try_from(STATS_DAYS).unwrap_or(7))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn snapshot(&self, recent_runs: usize) -> Result<Vec<RoutineSnapshot>> {
        let mut by_name: BTreeMap<String, RoutineSnapshot> = BTreeMap::new();
        let instances: Vec<(String, String, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> =
            sqlx::query_as(
                "SELECT routine, host_name, last_started_at, last_stopped_at
             FROM background_routine_instances ORDER BY routine, host_name",
            )
            .fetch_all(&self.pool)
            .await?;
        for (routine, host_name, last_started_at, last_stopped_at) in instances {
            snapshot_of(&mut by_name, routine)
                .instances
                .push(RoutineInstance {
                    host_name,
                    last_started_at,
                    last_stopped_at,
                });
        }

        let runs: Vec<(String, String, DateTime<Utc>, i64, Option<String>)> = sqlx::query_as(
            "SELECT routine, host_name, at, duration_ms, error FROM (
                 SELECT *, row_number() OVER (PARTITION BY routine ORDER BY id DESC) AS rn
                 FROM background_routine_runs) r
             WHERE rn <= $1 ORDER BY routine, id DESC",
        )
        .bind(i64::try_from(recent_runs).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        for (routine, host_name, at, duration_ms, error) in runs {
            snapshot_of(&mut by_name, routine)
                .recent_runs
                .push(RoutineRun {
                    at,
                    host_name,
                    duration_ms,
                    error,
                });
        }

        let stats: Vec<(String, NaiveDate, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT routine, day, run_count, error_count, min_duration_ms, max_duration_ms,
                    total_duration_ms
             FROM background_routine_stats
             WHERE day >= (now() AT TIME ZONE 'UTC')::date - ($1::int - 1)
             ORDER BY routine, day",
        )
        .bind(i32::try_from(STATS_DAYS).unwrap_or(7))
        .fetch_all(&self.pool)
        .await?;
        let mut days: BTreeMap<String, Vec<(NaiveDate, DayStats)>> = BTreeMap::new();
        for (routine, day, runs, errors, min, max, total) in stats {
            days.entry(routine).or_default().push((
                day,
                DayStats {
                    runs,
                    errors,
                    min,
                    max,
                    total,
                },
            ));
        }
        for (routine, d) in days {
            snapshot_of(&mut by_name, routine).stats = stats_from(d.into_iter());
        }
        Ok(by_name.into_values().collect())
    }
}

fn snapshot_of(m: &mut BTreeMap<String, RoutineSnapshot>, name: String) -> &mut RoutineSnapshot {
    m.entry(name.clone()).or_insert_with(|| RoutineSnapshot {
        name,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_instance() {
        let t = |s| Some(DateTime::<Utc>::from_timestamp(s, 0).unwrap());
        let i = |a, b| RoutineInstance {
            host_name: "h".into(),
            last_started_at: a,
            last_stopped_at: b,
        };
        assert!(i(t(1), None).is_running());
        assert!(i(t(2), t(1)).is_running());
        assert!(!i(t(1), t(2)).is_running());
        assert!(!i(None, t(2)).is_running());
    }

    #[test]
    fn stats_combine_days() {
        let d = |n| NaiveDate::from_ymd_opt(2026, 1, n).unwrap();
        let s = stats_from(
            [
                (
                    d(1),
                    DayStats {
                        runs: 2,
                        errors: 1,
                        min: 5,
                        max: 10,
                        total: 15,
                    },
                ),
                (
                    d(2),
                    DayStats {
                        runs: 1,
                        errors: 0,
                        min: 3,
                        max: 3,
                        total: 3,
                    },
                ),
            ]
            .into_iter(),
        );
        assert_eq!(s.since, day_start(d(1)));
        assert_eq!((s.run_count, s.error_count), (3, 1));
        assert_eq!(
            (s.min_duration_ms, s.avg_duration_ms, s.max_duration_ms),
            (3, 6, 10)
        );
    }

    #[tokio::test]
    async fn memory_recorder_is_bounded() {
        let r = MemoryRoutineRecorder::default();
        r.started("a", "h1").await.unwrap();
        for i in 0..(RECENT_RUNS_KEPT + 5) {
            let run = RoutineRun {
                at: Utc::now(),
                host_name: "h1".into(),
                duration_ms: i64::try_from(i).unwrap(),
                error: (i % 2 == 0).then(|| "boom".to_owned()),
            };
            r.record_run("a", &run).await.unwrap();
        }
        r.stopped("a", "h1").await.unwrap();
        let snap = r.snapshot(10).await.unwrap();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].recent_runs.len(), 10);
        assert_eq!(
            snap[0].recent_runs[0].duration_ms,
            i64::try_from(RECENT_RUNS_KEPT + 4).unwrap()
        );
        assert_eq!(
            snap[0].stats.run_count,
            i64::try_from(RECENT_RUNS_KEPT + 5).unwrap()
        );
        assert!(!snap[0].instances[0].is_running());
    }
}
