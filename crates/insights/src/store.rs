//! The trend tables (migration `20261114000001_insights`): series, their points per
//! repository (partitioned by year) and pre-summed per series, the recorded times, the
//! per-repository state counting continues from, and the backfill and recording queues.
//!
//! Creating a series sends `NOTIFY` [`CHANNEL`] on commit, which wakes the enqueuer.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Utc};
use roaring::RoaringBitmap;
use sourcegull_compute::ValueCounts;
use sourcegull_db::notify::notify;
use sourcegull_db::{Db, Fence, Result};
use sourcegull_pgqueue::QueueConfig;
use sourcegull_types::UserId;
use sqlx::postgres::PgRow;
use sqlx::types::Json;
use sqlx::{PgPool, Row};

use crate::series::{GenerationMethod, GroupBy, Interval, IntervalUnit, RepoScope};

/// `NOTIFY` channel: a series was created or deleted. Payload: the series id.
pub const CHANNEL: &str = "insights";
/// Queue table of backfill jobs (one per series and repository).
pub const BACKFILL_JOBS: &str = "insight_backfill_jobs";
/// Queue table of recording jobs (one per series and point time).
pub const RECORDING_JOBS: &str = "insight_recording_jobs";
/// The "other" value: what is left after the top values (and the count of `search` series).
pub const OTHER: &str = "";
/// Values stored per repository and point; the rest are summed into [`OTHER`].
pub const MAX_REPO_VALUES: usize = 1000;
/// Values kept per point of a series (the aggregate and the read path), plus [`OTHER`]:
/// capture-group series keep the top values.
pub const MAX_SERIES_VALUES: usize = 20;

/// Where a series' backfill is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackfillState {
    New,
    Running,
    Completed,
    Failed,
}

impl BackfillState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::New,
        }
    }
}

/// A trend series.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub id: i64,
    pub series_id: String,
    pub query: String,
    pub method: GenerationMethod,
    pub group_by: Option<GroupBy>,
    pub scope: RepoScope,
    pub interval: Interval,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub backfill_state: BackfillState,
    pub backfill_queued_at: Option<DateTime<Utc>>,
    pub backfill_completed_at: Option<DateTime<Utc>>,
    pub backfill_times: Vec<DateTime<Utc>>,
    pub repos_total: i32,
    pub repos_done: i32,
    pub repos_failed: i32,
    /// The repositories with points.
    pub repos: RoaringBitmap,
    pub last_recorded_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// The backfill priority an admin set (see [`Series::priority`]).
    pub backfill_priority: Option<i64>,
}

pub(crate) const SERIES_COLUMNS: &str = "id, series_id, query, generation_method, group_by, repo_scope, \
     repositories, repository_criteria, sample_interval_unit, sample_interval_value, created_by, \
     created_at, deleted_at, backfill_state, backfill_queued_at, backfill_completed_at, \
     backfill_times, repos_total, repos_done, repos_failed, repo_bitmap, last_recorded_at, error, \
     backfill_priority";

fn decode_err(column: &str, what: String) -> sqlx::Error {
    sqlx::Error::ColumnDecode {
        index: column.into(),
        source: what.into(),
    }
}

pub(crate) fn series_from_row(r: &PgRow) -> sqlx::Result<Series> {
    let method: String = r.try_get("generation_method")?;
    let group_by: Option<String> = r.try_get("group_by")?;
    let scope: String = r.try_get("repo_scope")?;
    let unit: String = r.try_get("sample_interval_unit")?;
    let value: i32 = r.try_get("sample_interval_value")?;
    let bitmap: Option<Vec<u8>> = r.try_get("repo_bitmap")?;
    let state: String = r.try_get("backfill_state")?;
    Ok(Series {
        id: r.try_get("id")?,
        series_id: r.try_get("series_id")?,
        query: r.try_get("query")?,
        method: GenerationMethod::parse(&method)
            .ok_or_else(|| decode_err("generation_method", method))?,
        group_by: group_by.as_deref().and_then(GroupBy::parse),
        scope: match scope.as_str() {
            "list" => RepoScope::List(r.try_get("repositories")?),
            "query" => RepoScope::Query(
                r.try_get::<Option<String>, _>("repository_criteria")?
                    .unwrap_or_default(),
            ),
            _ => RepoScope::All,
        },
        interval: Interval::new(
            IntervalUnit::parse(&unit).ok_or_else(|| decode_err("sample_interval_unit", unit))?,
            u32::try_from(value).unwrap_or(1),
        ),
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        deleted_at: r.try_get("deleted_at")?,
        backfill_state: BackfillState::parse(&state),
        backfill_queued_at: r.try_get("backfill_queued_at")?,
        backfill_completed_at: r.try_get("backfill_completed_at")?,
        backfill_times: r.try_get("backfill_times")?,
        repos_total: r.try_get("repos_total")?,
        repos_done: r.try_get("repos_done")?,
        repos_failed: r.try_get("repos_failed")?,
        repos: bitmap
            .and_then(|b| RoaringBitmap::deserialize_from(&b[..]).ok())
            .unwrap_or_default(),
        last_recorded_at: r.try_get("last_recorded_at")?,
        error: r.try_get("error")?,
        backfill_priority: r.try_get("backfill_priority")?,
    })
}

impl Series {
    /// The origin of the series' point grid: its creation, in whole seconds (points are
    /// counted at second precision).
    pub fn anchor(&self) -> DateTime<Utc> {
        chrono::DurationRound::duration_trunc(self.created_at, chrono::TimeDelta::seconds(1))
            .unwrap_or(self.created_at)
    }

    /// The priority of the series' backfill jobs (higher first): the one an admin set, else
    /// the creation time in seconds, so newer series go first.
    pub fn priority(&self) -> i64 {
        self.backfill_priority
            .unwrap_or_else(|| self.created_at.timestamp())
    }
}

/// A series to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSeries {
    pub query: String,
    pub method: GenerationMethod,
    pub group_by: Option<GroupBy>,
    pub scope: RepoScope,
    pub interval: Interval,
    pub created_by: Option<UserId>,
}

/// The payload of a backfill job.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackfillJob {
    pub series_id: i64,
    pub repo_id: i32,
}

/// The payload of a recording job.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RecordingJob {
    pub series_id: i64,
    pub time: DateTime<Utc>,
}

/// The newest point counted for a series in one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub repo_id: i32,
    pub time: DateTime<Utc>,
    /// Empty when the repository had no commit at `time`.
    pub commit: String,
    /// Every value (not only the top ones).
    pub counts: ValueCounts,
    /// When `commit` was resolved; the refs not changing since means it is still current.
    pub checked_at: DateTime<Utc>,
}

/// One repository's result at one point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPoint {
    pub repo_id: i32,
    pub time: DateTime<Utc>,
    pub counts: ValueCounts,
}

/// Backfill progress of a series, from its jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    /// Queued or running.
    pub pending: i64,
}

/// One point value of a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointValue {
    pub time: DateTime<Utc>,
    /// [`OTHER`] for a `search` series' count and for the other values of the others.
    pub value: String,
    pub count: i64,
}

/// The top [`MAX_SERIES_VALUES`] values of one point (by count, then value), with the rest
/// (and any [`OTHER`] already present) summed into [`OTHER`].
pub fn top_values(counts: impl IntoIterator<Item = (String, i64)>, n: usize) -> Vec<(String, i64)> {
    let mut other = 0i64;
    let mut values: Vec<(String, i64)> = Vec::new();
    for (v, c) in counts {
        if v == OTHER {
            other += c;
        } else if c > 0 {
            values.push((v, c));
        }
    }
    values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if values.len() > n {
        other += values.drain(n..).map(|(_, c)| c).sum::<i64>();
    }
    if other > 0 || values.is_empty() {
        values.push((OTHER.to_owned(), other));
    }
    values
}

fn counts_to_rows(counts: &ValueCounts) -> Vec<(String, i64)> {
    let mut rows = top_values(
        counts
            .0
            .iter()
            .map(|(v, c)| (v.clone(), i64::try_from(*c).unwrap_or(i64::MAX))),
        MAX_REPO_VALUES,
    );
    // Zero rows are not stored: a missing row counts 0.
    rows.retain(|(_, c)| *c > 0);
    rows
}

/// The `repositories` and `repository_criteria` columns of a scope.
fn scope_columns(scope: &RepoScope) -> (Vec<String>, Option<String>) {
    match scope {
        RepoScope::All => (Vec::new(), None),
        RepoScope::List(names) => (names.clone(), None),
        RepoScope::Query(q) => (Vec::new(), Some(q.clone())),
    }
}

fn counts_json(c: &ValueCounts) -> Json<BTreeMap<String, u64>> {
    Json(c.0.clone())
}

fn bitmap_bytes(b: &RoaringBitmap) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.serialized_size());
    // Writing to a Vec cannot fail.
    let _ = b.serialize_into(&mut out);
    out
}

/// A row of `insight_series_repos`.
type StateRow = (
    i32,
    DateTime<Utc>,
    String,
    Json<BTreeMap<String, u64>>,
    DateTime<Utc>,
);

/// The trend tables.
#[derive(Debug, Clone)]
pub struct InsightStore {
    pool: PgPool,
    /// Years whose points partition is known to exist.
    partitions: Arc<Mutex<HashSet<i32>>>,
}

impl InsightStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
            partitions: Arc::default(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Queue settings of the backfill jobs: newest series first (`priority`).
    pub fn backfill_queue_config() -> QueueConfig {
        let mut c = QueueConfig::new(BACKFILL_JOBS).with_priority();
        c.max_failures = 5;
        c
    }

    pub fn recording_queue_config() -> QueueConfig {
        let mut c = QueueConfig::new(RECORDING_JOBS);
        c.max_failures = 5;
        c
    }

    pub async fn create(&self, s: &NewSeries) -> Result<Series> {
        let mut tx = self.pool.begin().await?;
        let series = Self::create_in(&mut tx, s).await?;
        tx.commit().await?;
        Ok(series)
    }

    /// [`InsightStore::create`] inside a transaction (the enqueuer is woken on commit).
    pub async fn create_in(tx: &mut sqlx::PgConnection, s: &NewSeries) -> Result<Series> {
        let (repositories, criteria) = scope_columns(&s.scope);
        let row = sqlx::query(&format!(
            "INSERT INTO insight_series (series_id, query, generation_method, group_by, \
                 repo_scope, repositories, repository_criteria, sample_interval_unit, \
                 sample_interval_value, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING {SERIES_COLUMNS}"
        ))
        .bind(uuid::Uuid::new_v4().simple().to_string())
        .bind(&s.query)
        .bind(s.method.as_str())
        .bind(s.group_by.map(GroupBy::as_str))
        .bind(s.scope.kind())
        .bind(&repositories)
        .bind(criteria)
        .bind(s.interval.unit.as_str())
        .bind(i32::try_from(s.interval.value).unwrap_or(i32::MAX))
        .bind(s.created_by)
        .fetch_one(&mut *tx)
        .await?;
        let series = series_from_row(&row)?;
        notify(&mut *tx, CHANNEL, &series.id.to_string()).await?;
        Ok(series)
    }

    /// A live series defined exactly as `s` (query, generation method, group, repository
    /// scope and interval), the oldest first: identical series are shared by every view that
    /// defines them.
    pub async fn find_matching_in(
        tx: &mut sqlx::PgConnection,
        s: &NewSeries,
    ) -> Result<Option<Series>> {
        let (repositories, criteria) = scope_columns(&s.scope);
        let row = sqlx::query(&format!(
            "SELECT {SERIES_COLUMNS} FROM insight_series
             WHERE deleted_at IS NULL AND query = $1 AND generation_method = $2
               AND group_by IS NOT DISTINCT FROM $3 AND repo_scope = $4 AND repositories = $5
               AND repository_criteria IS NOT DISTINCT FROM $6
               AND sample_interval_unit = $7 AND sample_interval_value = $8
             ORDER BY id LIMIT 1"
        ))
        .bind(&s.query)
        .bind(s.method.as_str())
        .bind(s.group_by.map(GroupBy::as_str))
        .bind(s.scope.kind())
        .bind(&repositories)
        .bind(criteria)
        .bind(s.interval.unit.as_str())
        .bind(i32::try_from(s.interval.value).unwrap_or(i32::MAX))
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.as_ref().map(series_from_row).transpose()?)
    }

    /// The series with these ids (deleted ones included), in no particular order.
    pub async fn get_many(&self, ids: &[i64]) -> Result<Vec<Series>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERIES_COLUMNS} FROM insight_series WHERE id = ANY($1)"
        ))
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(series_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    /// The series `id`, unless deleted.
    pub async fn get(&self, id: i64) -> Result<Option<Series>> {
        let row = sqlx::query(&format!(
            "SELECT {SERIES_COLUMNS} FROM insight_series WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(series_from_row).transpose()?)
    }

    /// Every series not deleted, oldest first.
    pub async fn list_live(&self) -> Result<Vec<Series>> {
        let rows = sqlx::query(&format!(
            "SELECT {SERIES_COLUMNS} FROM insight_series WHERE deleted_at IS NULL ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(series_from_row)
            .collect::<sqlx::Result<_>>()?)
    }

    /// Marks series `id` deleted and cancels its queued jobs; the janitor removes it later.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let deleted = Self::delete_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// [`InsightStore::delete`] inside a transaction.
    pub async fn delete_in(tx: &mut sqlx::PgConnection, id: i64) -> Result<bool> {
        let n = sqlx::query(
            "UPDATE insight_series SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        for table in [BACKFILL_JOBS, RECORDING_JOBS] {
            sqlx::query(&format!(
                "UPDATE {table} SET cancel = true,
                     state = CASE WHEN state = 'queued' THEN 'canceled' ELSE state END,
                     finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
                 WHERE series_id = $1 AND state IN ('queued', 'processing')"
            ))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        Ok(n > 0)
    }

    /// Marks a series that cannot be counted, while `fence` is current.
    pub async fn set_failed(&self, id: i64, error: &str, fence: &Fence) -> Result<()> {
        let mut tx = fence.begin(&self.pool).await?;
        sqlx::query(
            "UPDATE insight_series SET backfill_state = 'failed', error = $2,
                 backfill_completed_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(error)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Starts the backfill of series `id`: one job per repository at the given point
    /// times. Newer series get a higher priority. Writes nothing when `fence` is no longer
    /// current.
    pub async fn start_backfill(
        &self,
        id: i64,
        times: &[DateTime<Utc>],
        repos: &[i32],
        priority: i64,
        fence: &Fence,
    ) -> Result<()> {
        let mut tx = fence.begin(&self.pool).await?;
        sqlx::query(
            "UPDATE insight_series SET backfill_state = 'running', backfill_queued_at = now(),
                 backfill_times = $2, repos_total = $3, repos_done = 0, repos_failed = 0,
                 error = NULL
             WHERE id = $1",
        )
        .bind(id)
        .bind(times)
        .bind(i32::try_from(repos.len()).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO insight_backfill_jobs (series_id, repo_id, priority)
             SELECT $1, r, $3 FROM unnest($2::int[]) AS r
             ON CONFLICT (series_id, repo_id) DO NOTHING",
        )
        .bind(id)
        .bind(repos)
        .bind(priority)
        .execute(&mut *tx)
        .await?;
        sourcegull_pgqueue::notify_enqueued(&mut *tx, BACKFILL_JOBS).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Backfill progress of series `id`.
    pub async fn progress(&self, id: i64) -> Result<Progress> {
        let (total, completed, failed, pending): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT count(*),
                    count(*) FILTER (WHERE state = 'completed'),
                    count(*) FILTER (WHERE state IN ('failed', 'canceled')),
                    count(*) FILTER (WHERE state IN ('queued', 'processing', 'errored'))
             FROM insight_backfill_jobs WHERE series_id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(Progress {
            total,
            completed,
            failed,
            pending,
        })
    }

    /// Series whose backfill runs but whose jobs have all finished.
    pub async fn backfills_done(&self) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT s.id FROM insight_series s
             WHERE s.deleted_at IS NULL AND s.backfill_state = 'running'
               AND NOT EXISTS (
                   SELECT 1 FROM insight_backfill_jobs j
                   WHERE j.series_id = s.id AND j.state IN ('queued', 'processing', 'errored'))",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Creates the yearly points partitions `times` fall into.
    async fn ensure_partitions(&self, times: &[DateTime<Utc>]) -> Result<()> {
        let years: Vec<i32> = {
            let known = self.partitions.lock().unwrap_or_else(|e| e.into_inner());
            let mut ys: Vec<i32> = times
                .iter()
                .map(Datelike::year)
                .filter(|y| !known.contains(y))
                .collect();
            ys.sort_unstable();
            ys.dedup();
            ys
        };
        if years.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        // Concurrent writers would race on CREATE TABLE IF NOT EXISTS.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('insight_series_points_partitions'))")
            .execute(&mut *tx)
            .await?;
        for y in &years {
            sqlx::query(&format!(
                "CREATE TABLE IF NOT EXISTS insight_series_points_y{y}
                     PARTITION OF insight_series_points
                     FOR VALUES FROM ('{y:04}-01-01 00:00:00+00') TO ('{:04}-01-01 00:00:00+00')",
                y + 1
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.partitions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(years);
        Ok(())
    }

    /// Writes points (replacing any at the same series, repository and time) and the
    /// repositories' states, in one transaction. A state is only moved forward.
    pub async fn write_points(
        &self,
        series_id: i64,
        points: &[RepoPoint],
        states: &[RepoState],
    ) -> Result<()> {
        let times: Vec<DateTime<Utc>> = points.iter().map(|p| p.time).collect();
        self.ensure_partitions(&times).await?;
        let (mut repo_ids, mut ts, mut values, mut counts) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for p in points {
            for (v, c) in counts_to_rows(&p.counts) {
                repo_ids.push(p.repo_id);
                ts.push(p.time);
                values.push(v);
                counts.push(c);
            }
        }
        let del_repos: Vec<i32> = points.iter().map(|p| p.repo_id).collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM insight_series_points p
             USING unnest($2::int[], $3::timestamptz[]) AS d(repo_id, time)
             WHERE p.series_id = $1 AND p.repo_id = d.repo_id AND p.time = d.time",
        )
        .bind(series_id)
        .bind(&del_repos)
        .bind(&times)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO insight_series_points (series_id, repo_id, time, value, count)
             SELECT $1, * FROM unnest($2::int[], $3::timestamptz[], $4::text[], $5::bigint[])",
        )
        .bind(series_id)
        .bind(&repo_ids)
        .bind(&ts)
        .bind(&values)
        .bind(&counts)
        .execute(&mut *tx)
        .await?;
        for s in states {
            sqlx::query(
                "INSERT INTO insight_series_repos (series_id, repo_id, time, commit_oid, counts,
                     checked_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (series_id, repo_id) DO UPDATE SET
                     time = EXCLUDED.time, commit_oid = EXCLUDED.commit_oid,
                     counts = EXCLUDED.counts, checked_at = EXCLUDED.checked_at
                 WHERE insight_series_repos.time <= EXCLUDED.time",
            )
            .bind(series_id)
            .bind(s.repo_id)
            .bind(s.time)
            .bind(&s.commit)
            .bind(counts_json(&s.counts))
            .bind(s.checked_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The per-repository states of series `id`.
    pub async fn repo_states(&self, id: i64) -> Result<Vec<RepoState>> {
        let rows: Vec<StateRow> = sqlx::query_as(
            "SELECT repo_id, time, commit_oid, counts, checked_at
                 FROM insight_series_repos WHERE series_id = $1 ORDER BY repo_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(repo_id, time, commit, Json(counts), checked_at)| RepoState {
                    repo_id,
                    time,
                    commit,
                    counts: ValueCounts(counts),
                    checked_at,
                },
            )
            .collect())
    }

    /// When the refs of each of `repos` last changed (repositories never fetched since
    /// cloning, or unknown to the repo nodes, are missing).
    pub async fn repos_last_changed(&self, repos: &[i32]) -> Result<HashMap<i32, DateTime<Utc>>> {
        let rows: Vec<(i32, DateTime<Utc>)> = sqlx::query_as(
            "SELECT repo_id, last_changed FROM gitserver_repos
             WHERE repo_id = ANY($1) AND last_changed IS NOT NULL",
        )
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Of `repos`, those the repo nodes report as not cloned (not countable yet).
    pub async fn not_cloned(&self, repos: &[i32]) -> Result<HashSet<i32>> {
        let rows: Vec<i32> = sqlx::query_scalar(
            "SELECT repo_id FROM gitserver_repos
             WHERE repo_id = ANY($1) AND clone_status <> 'cloned'",
        )
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Recomputes the aggregate of series `id` at `times` from its per-repository points,
    /// records the times, and refreshes the series' repository bitmap and newest time.
    async fn aggregate(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
        id: i64,
        times: &[DateTime<Utc>],
    ) -> Result<()> {
        let sums: Vec<(DateTime<Utc>, String, i64)> = sqlx::query_as(
            "SELECT time, value, sum(count)::bigint FROM insight_series_points
             WHERE series_id = $1 AND time = ANY($2)
             GROUP BY time, value",
        )
        .bind(id)
        .bind(times)
        .fetch_all(&mut **tx)
        .await?;
        let mut by_time: BTreeMap<DateTime<Utc>, Vec<(String, i64)>> =
            times.iter().map(|t| (*t, Vec::new())).collect();
        for (t, v, c) in sums {
            by_time.entry(t).or_default().push((v, c));
        }
        let (mut ts, mut values, mut counts) = (Vec::new(), Vec::new(), Vec::new());
        for (t, vs) in by_time {
            for (v, c) in top_values(vs, MAX_SERIES_VALUES) {
                ts.push(t);
                values.push(v);
                counts.push(c);
            }
        }
        sqlx::query(
            "DELETE FROM insight_series_points_agg WHERE series_id = $1 AND time = ANY($2)",
        )
        .bind(id)
        .bind(times)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO insight_series_points_agg (series_id, time, value, count)
             SELECT $1, * FROM unnest($2::timestamptz[], $3::text[], $4::bigint[])",
        )
        .bind(id)
        .bind(&ts)
        .bind(&values)
        .bind(&counts)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO insight_series_times (series_id, time)
             SELECT $1, t FROM unnest($2::timestamptz[]) AS t ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(times)
        .execute(&mut **tx)
        .await?;
        let repos: Vec<i32> =
            sqlx::query_scalar("SELECT repo_id FROM insight_series_repos WHERE series_id = $1")
                .bind(id)
                .fetch_all(&mut **tx)
                .await?;
        let bitmap: RoaringBitmap = repos
            .iter()
            .filter_map(|r| u32::try_from(*r).ok())
            .collect();
        sqlx::query(
            "UPDATE insight_series SET repo_bitmap = $2,
                 last_recorded_at = GREATEST(last_recorded_at, $3)
             WHERE id = $1",
        )
        .bind(id)
        .bind(bitmap_bytes(&bitmap))
        .bind(times.iter().max())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Completes the backfill of `series`: sums its points per backfill time into the
    /// aggregate and records the progress from the jobs, while `fence` is current.
    pub async fn finish_backfill(&self, series: &Series, fence: &Fence) -> Result<()> {
        let p = self.progress(series.id).await?;
        let mut tx = fence.begin(&self.pool).await?;
        self.aggregate(&mut tx, series.id, &series.backfill_times)
            .await?;
        sqlx::query(
            "UPDATE insight_series SET backfill_state = 'completed', backfill_completed_at = now(),
                 repos_done = $2, repos_failed = $3
             WHERE id = $1 AND backfill_state = 'running'",
        )
        .bind(series.id)
        .bind(i32::try_from(p.completed).unwrap_or(i32::MAX))
        .bind(i32::try_from(p.failed).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Completes a recording of series `id` at `time`: the aggregate at that time.
    pub async fn finish_recording(&self, id: i64, time: DateTime<Utc>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        self.aggregate(&mut tx, id, &[time]).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Queues a recording of series `id` at `time` (once per time), while `fence` is current.
    pub async fn enqueue_recording(
        &self,
        id: i64,
        time: DateTime<Utc>,
        fence: &Fence,
    ) -> Result<bool> {
        let mut tx = fence.begin(&self.pool).await?;
        let n = sqlx::query(
            "INSERT INTO insight_recording_jobs (series_id, time) VALUES ($1, $2)
             ON CONFLICT (series_id, time) DO NOTHING",
        )
        .bind(id)
        .bind(time)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sourcegull_pgqueue::notify_enqueued(&mut *tx, RECORDING_JOBS).await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// The newest queued or running recording time of series `id`.
    pub async fn pending_recording(&self, id: i64) -> Result<Option<DateTime<Utc>>> {
        Ok(sqlx::query_scalar(
            "SELECT max(time) FROM insight_recording_jobs
             WHERE series_id = $1 AND state IN ('queued', 'processing', 'errored')",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The point times of series `id`, oldest first.
    pub async fn times(&self, id: i64) -> Result<Vec<DateTime<Utc>>> {
        Ok(sqlx::query_scalar(
            "SELECT time FROM insight_series_times WHERE series_id = $1 ORDER BY time",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The pre-summed points of series `id`.
    pub async fn aggregate_points(&self, id: i64) -> Result<Vec<PointValue>> {
        let rows: Vec<(DateTime<Utc>, String, i64)> = sqlx::query_as(
            "SELECT time, value, count FROM insight_series_points_agg
             WHERE series_id = $1 ORDER BY time, value",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(time, value, count)| PointValue { time, value, count })
            .collect())
    }

    /// Points of series `id` summed over `repos` only, per time and value.
    pub async fn repo_sums(&self, id: i64, repos: &[i32]) -> Result<Vec<PointValue>> {
        let rows: Vec<(DateTime<Utc>, String, i64)> = sqlx::query_as(
            "SELECT time, value, sum(count)::bigint FROM insight_series_points
             WHERE series_id = $1 AND repo_id = ANY($2)
             GROUP BY time, value ORDER BY time, value",
        )
        .bind(id)
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(time, value, count)| PointValue { time, value, count })
            .collect())
    }

    /// The per-repository points of series `id` in `repos`, by time, repository and value.
    pub async fn repo_points(
        &self,
        id: i64,
        repos: &[i32],
    ) -> Result<Vec<(DateTime<Utc>, i32, String, i64)>> {
        Ok(sqlx::query_as(
            "SELECT time, repo_id, value, count FROM insight_series_points
             WHERE series_id = $1 AND repo_id = ANY($2)
             ORDER BY time, repo_id, value",
        )
        .bind(id)
        .bind(repos)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Rows of series `id` in the per-repository points (`totalPoints`).
    pub async fn point_rows(&self, id: i64) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM insight_series_points WHERE series_id = $1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// The repositories whose backfill job failed for good.
    pub async fn failed_repos(&self, id: i64) -> Result<Vec<i32>> {
        Ok(sqlx::query_scalar(
            "SELECT repo_id FROM insight_backfill_jobs
             WHERE series_id = $1 AND state = 'failed' ORDER BY repo_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Removes series deleted before `before` with everything they hold.
    pub async fn purge_deleted(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(
            sqlx::query("DELETE FROM insight_series WHERE deleted_at < $1")
                .bind(before)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Keeps the newest `max_points` times of every series; older points go.
    pub async fn prune_points(&self, max_points: i64) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let cutoffs: Vec<(i64, DateTime<Utc>)> = sqlx::query_as(
            "SELECT series_id, time FROM (
                 SELECT series_id, time,
                        row_number() OVER (PARTITION BY series_id ORDER BY time DESC) AS n
                 FROM insight_series_times) t
             WHERE n = $1 + 1",
        )
        .bind(max_points)
        .fetch_all(&mut *tx)
        .await?;
        let mut removed = 0;
        for (series, cutoff) in cutoffs {
            for table in [
                "insight_series_points",
                "insight_series_points_agg",
                "insight_series_times",
            ] {
                removed += sqlx::query(&format!(
                    "DELETE FROM {table} WHERE series_id = $1 AND time <= $2"
                ))
                .bind(series)
                .bind(cutoff)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            }
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// Deletes finished jobs older than `before` (completed backfills keep their progress
    /// in the series row).
    pub async fn delete_finished_jobs(&self, before: DateTime<Utc>) -> Result<u64> {
        let mut n = sqlx::query(
            "DELETE FROM insight_backfill_jobs j USING insight_series s
             WHERE j.series_id = s.id AND s.backfill_state IN ('completed', 'failed')
               AND j.finished_at < $1",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        n += sqlx::query(
            "DELETE FROM insight_recording_jobs
             WHERE finished_at < $1 AND state IN ('completed', 'failed', 'canceled')",
        )
        .bind(before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_values_fold_the_rest_into_other() {
        let vs = vec![
            ("a".to_owned(), 5),
            ("b".to_owned(), 9),
            ("c".to_owned(), 5),
            (OTHER.to_owned(), 2),
            ("d".to_owned(), 1),
        ];
        assert_eq!(
            top_values(vs.clone(), 2),
            vec![("b".into(), 9), ("a".into(), 5), (OTHER.into(), 8)]
        );
        assert_eq!(top_values(vs, 10).len(), 5);
        // A point without values keeps a zero count.
        assert_eq!(top_values(Vec::new(), 3), vec![(OTHER.into(), 0)]);
        let mut c = ValueCounts::default();
        c.0.insert(String::new(), 0);
        assert!(counts_to_rows(&c).is_empty());
    }
}
