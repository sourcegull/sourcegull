//! The usage rollup job: a leader-elected singleton of the worker role.
//!
//! Each run rolls up every UTC day from the oldest raw event to today that is not yet
//! complete. A day is recomputed as a whole in one transaction (delete its rows, aggregate
//! `event_logs` again), so reruns and overlapping runs give the same rows. A day rolled up
//! after it ended (plus [`GRACE`]) is complete and left alone afterwards; today is rolled up
//! again on every run. The active users of the days, weeks and months that contain the
//! rolled-up days are then recomputed from the daily rows.
//!
//! After the rollup, [`prune`] deletes raw events older than `eventLogs.retention` (never
//! from a day that is not complete) and rollups older than `eventLogs.rollupRetention`.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, Utc};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{Db, DbError, Fence, sqlx};
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::names;

/// Lease name of the rollup job.
pub const ROLLUP: &str = "usage-analytics-rollup";
/// How often the rollup runs.
pub const ROLLUP_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// A day is complete once it ended this long ago (late inserts of the last requests).
pub const GRACE: chrono::Duration = chrono::Duration::minutes(5);
/// The most days one run rolls up (oldest first); the next run continues.
pub const MAX_DAYS_PER_RUN: usize = 400;
/// Raw events deleted per statement.
const PRUNE_BATCH: i64 = 10_000;
/// Counter of raw usage events deleted.
pub const EVENTS_PRUNED_METRIC: &str = metric_names::EVENT_LOGS_PRUNED_TOTAL;
/// Counter of days rolled up.
pub const DAYS_ROLLED_UP_METRIC: &str = metric_names::EVENT_LOGS_DAYS_ROLLED_UP_TOTAL;

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupStats {
    /// The days rolled up, oldest first.
    pub days: Vec<NaiveDate>,
    /// Active-user periods recomputed.
    pub periods: usize,
}

/// A period of the active users table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    /// The value in `event_logs_active_users.period`.
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Day => "day",
            Period::Week => "week",
            Period::Month => "month",
        }
    }

    /// The first day of the period containing `day` (weeks start on Monday).
    pub fn start(self, day: NaiveDate) -> NaiveDate {
        match self {
            Period::Day => day,
            Period::Week => day - Days::new(u64::from(day.weekday().num_days_from_monday())),
            Period::Month => day.with_day(1).expect("day 1 exists"),
        }
    }

    /// The first day after the period starting at `start`.
    pub fn end(self, start: NaiveDate) -> NaiveDate {
        match self {
            Period::Day => start + Days::new(1),
            Period::Week => start + Days::new(7),
            Period::Month => start + Months::new(1),
        }
    }
}

/// The start of a UTC day.
pub fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc()
}

/// The periods whose active users change when `days` are rolled up.
pub fn touched_periods(days: &[NaiveDate]) -> BTreeSet<(Period, NaiveDate)> {
    days.iter()
        .flat_map(|&d| [Period::Day, Period::Week, Period::Month].map(|p| (p, p.start(d))))
        .collect()
}

/// The days from the oldest raw event to `today` that are not complete, oldest first, at
/// most [`MAX_DAYS_PER_RUN`].
async fn pending_days(db: &Db, today: NaiveDate) -> Result<Vec<NaiveDate>, DbError> {
    let oldest: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT min(timestamp) FROM event_logs")
        .fetch_one(db.pool())
        .await?;
    let first = oldest.map_or(today, |t| t.date_naive().min(today));
    let days: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT d::date FROM generate_series($1::date, $2::date, interval '1 day') AS d
         WHERE NOT EXISTS (
             SELECT 1 FROM event_logs_rollup_days r WHERE r.day = d::date AND r.complete)
         ORDER BY 1
         LIMIT $3",
    )
    .bind(first)
    .bind(today)
    .bind(MAX_DAYS_PER_RUN as i64)
    .fetch_all(db.pool())
    .await?;
    Ok(days)
}

/// Recomputes the rollups of one day, while `fence` is current.
async fn rollup_day(
    db: &Db,
    day: NaiveDate,
    now: DateTime<Utc>,
    fence: &Fence,
) -> Result<(), DbError> {
    let from = day_start(day);
    let to = day_start(day + Days::new(1));
    let mut tx = fence.begin(db.pool()).await?;
    for table in [
        "event_logs_daily",
        "event_logs_search_daily",
        "event_logs_codeintel_daily",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE day = $1"))
            .bind(day)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO event_logs_daily (day, name, source, user_id, anonymous_user_id, count)
         SELECT $1, name, source, user_id,
                CASE WHEN user_id IS NULL THEN anonymous_user_id ELSE '' END, count(*)
         FROM event_logs
         WHERE timestamp >= $2 AND timestamp < $3
         GROUP BY 2, 3, 4, 5",
    )
    .bind(day)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO event_logs_search_daily (day, search_type, latency_bucket_ms, count)
         SELECT $1, t, b, count(*)
         FROM (
             SELECT left(coalesce(nullif(argument->>'type', ''), 'unknown'), 64) AS t,
                    CASE WHEN jsonb_typeof(argument->'durationMs') = 'number' THEN
                        coalesce((SELECT min(x) FROM unnest($5::int[]) AS x
                                  WHERE x >= (argument->>'durationMs')::float8), 2147483647)
                    ELSE -1 END AS b
             FROM event_logs
             WHERE name = $4 AND timestamp >= $2 AND timestamp < $3
         ) s
         GROUP BY t, b",
    )
    .bind(day)
    .bind(from)
    .bind(to)
    .bind(names::SEARCH_RESULTS_QUERIED)
    .bind(names::LATENCY_BUCKETS_MS)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO event_logs_codeintel_daily (day, name, repo_name, language, count)
         SELECT $1, name, left(coalesce(argument->>'repoName', ''), 512),
                left(coalesce(argument->>'languageId', ''), 64), count(*)
         FROM event_logs
         WHERE name LIKE 'codeintel.%' AND timestamp >= $2 AND timestamp < $3
         GROUP BY 2, 3, 4",
    )
    .bind(day)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO event_logs_rollup_days (day, rolled_up_at, complete) VALUES ($1, $2, $3)
         ON CONFLICT (day) DO UPDATE
             SET rolled_up_at = EXCLUDED.rolled_up_at, complete = EXCLUDED.complete",
    )
    .bind(day)
    .bind(now)
    .bind(now >= to + GRACE)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Recomputes the active users of one period from the daily rollup, while `fence` is current.
async fn rollup_period(
    db: &Db,
    period: Period,
    start: NaiveDate,
    fence: &Fence,
) -> Result<(), DbError> {
    let mut tx = fence.begin(db.pool()).await?;
    sqlx::query(
        "INSERT INTO event_logs_active_users
             (period, period_start, users, registered_users, anonymous_users)
         SELECT $1, $2,
                count(DISTINCT coalesce('u' || user_id::text, 'a' || anonymous_user_id)),
                count(DISTINCT user_id),
                count(DISTINCT anonymous_user_id) FILTER (WHERE user_id IS NULL)
         FROM event_logs_daily
         WHERE day >= $2 AND day < $3
         ON CONFLICT (period, period_start) DO UPDATE
             SET users = EXCLUDED.users, registered_users = EXCLUDED.registered_users,
                 anonymous_users = EXCLUDED.anonymous_users",
    )
    .bind(period.as_str())
    .bind(start)
    .bind(period.end(start))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Rolls up every pending day (see the module docs) as of `now`. Each day and period is
/// written only while `fence` is current ([`DbError::Fenced`] otherwise).
pub async fn rollup(db: &Db, now: DateTime<Utc>, fence: &Fence) -> Result<RollupStats, DbError> {
    let days = pending_days(db, now.date_naive()).await?;
    for &day in &days {
        rollup_day(db, day, now, fence).await?;
    }
    let periods = touched_periods(&days);
    for &(period, start) in &periods {
        rollup_period(db, period, start, fence).await?;
    }
    metrics::counter!(DAYS_ROLLED_UP_METRIC).increment(days.len() as u64);
    Ok(RollupStats {
        periods: periods.len(),
        days,
    })
}

/// What [`prune`] deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Raw events.
    pub events: u64,
    /// Rollup rows.
    pub rollups: u64,
}

fn sub(now: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(d)
        .ok()
        .and_then(|d| now.checked_sub_signed(d))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Deletes raw events older than `retention` (but none of a day that is not rolled up
/// completely) and rollups older than `rollup_retention` (at least `retention`).
pub async fn prune(
    db: &Db,
    now: DateTime<Utc>,
    retention: Duration,
    rollup_retention: Duration,
) -> Result<PruneStats, DbError> {
    let mut cutoff = sub(now, retention);
    if let Some(&first_pending) = pending_days(db, now.date_naive()).await?.first() {
        cutoff = cutoff.min(day_start(first_pending));
    }
    let mut stats = PruneStats::default();
    loop {
        let n = sqlx::query(
            "DELETE FROM event_logs WHERE id IN (
                 SELECT id FROM event_logs WHERE timestamp < $1 ORDER BY timestamp LIMIT $2)",
        )
        .bind(cutoff)
        .bind(PRUNE_BATCH)
        .execute(db.pool())
        .await?
        .rows_affected();
        stats.events += n;
        if n < PRUNE_BATCH as u64 {
            break;
        }
    }
    let rollup_cutoff = sub(now, rollup_retention.max(retention)).date_naive();
    for table in [
        "event_logs_daily",
        "event_logs_search_daily",
        "event_logs_codeintel_daily",
        "event_logs_rollup_days",
    ] {
        stats.rollups += sqlx::query(&format!("DELETE FROM {table} WHERE day < $1"))
            .bind(rollup_cutoff)
            .execute(db.pool())
            .await?
            .rows_affected();
    }
    // Periods that ended before the cutoff.
    stats.rollups += sqlx::query(
        "DELETE FROM event_logs_active_users
         WHERE CASE period WHEN 'day' THEN period_start + 1
                           WHEN 'week' THEN period_start + 7
                           ELSE (period_start + interval '1 month')::date END <= $1",
    )
    .bind(rollup_cutoff)
    .execute(db.pool())
    .await?
    .rows_affected();
    metrics::counter!(EVENTS_PRUNED_METRIC).increment(stats.events);
    Ok(stats)
}

/// Runs [`rollup`] and [`prune`] every [`ROLLUP_INTERVAL`] (first run right away) until
/// `cancel` fires.
pub async fn run_rollup(db: Db, config: SiteConfigHandle, fence: Fence, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(ROLLUP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let now = Utc::now();
        match rollup(&db, now, &fence).await {
            Ok(s) if !s.days.is_empty() => {
                tracing::debug!(
                    days = s.days.len(),
                    periods = s.periods,
                    "rolled up usage events"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "rolling up usage events failed");
                continue;
            }
        }
        let (retention, rollup_retention) = {
            let c = config.load();
            (
                c.value.event_logs_retention.get(),
                c.value.event_logs_rollup_retention.get(),
            )
        };
        match prune(&db, now, retention, rollup_retention).await {
            Ok(s) if s.events + s.rollups > 0 => {
                tracing::info!(
                    events = s.events,
                    rollups = s.rollups,
                    "deleted old usage events"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "deleting old usage events failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn period_bounds() {
        // 2026-10-21 is a Wednesday.
        assert_eq!(Period::Week.start(d("2026-10-21")), d("2026-10-19"));
        assert_eq!(Period::Week.start(d("2026-10-19")), d("2026-10-19"));
        assert_eq!(Period::Week.start(d("2026-10-25")), d("2026-10-19"));
        assert_eq!(Period::Week.end(d("2026-10-19")), d("2026-10-26"));
        assert_eq!(Period::Month.start(d("2026-10-21")), d("2026-10-01"));
        assert_eq!(Period::Month.end(d("2026-12-01")), d("2027-01-01"));
        assert_eq!(Period::Day.end(d("2026-02-28")), d("2026-03-01"));
    }

    #[test]
    fn touched_periods_dedupe() {
        let p = touched_periods(&[d("2026-10-19"), d("2026-10-20"), d("2026-11-02")]);
        let weeks: Vec<_> = p.iter().filter(|(k, _)| *k == Period::Week).collect();
        let months: Vec<_> = p.iter().filter(|(k, _)| *k == Period::Month).collect();
        assert_eq!(p.iter().filter(|(k, _)| *k == Period::Day).count(), 3);
        assert_eq!(weeks.len(), 2);
        assert_eq!(months.len(), 2);
    }
}
