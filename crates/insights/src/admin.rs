//! The admin backfill queue (prioritised, resumable): `insightAdminBackfillQueue` with
//! `retryInsightSeriesBackfill` and `moveInsightSeriesBackfillToFrontOfQueue`/`…BackOfQueue`.
//!
//! A queue item is one series on one view (a series is listed once per view it is on). Its
//! state comes from the series' backfill and its jobs: `new` until the enqueuer
//! queued the jobs, then queued until a job starts, processing until every job finished, then
//! completed or failed. The queue position ranks the unfinished backfills in the order the
//! workers take their jobs (priority, then when queued). An admin moves a backfill to the
//! front or back by giving the series a priority above or below every other unfinished
//! backfill; its queued jobs take the new priority at once.

use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_db::Result;
use sourcegull_db::notify::notify;
use sourcegull_types::UserId;
use sqlx::Row;

use crate::store::{BACKFILL_JOBS, CHANNEL, InsightStore};

/// The effective backfill priority of a series row `s` (as [`crate::Series::priority`]).
const PRIORITY: &str =
    "coalesce(s.backfill_priority, floor(extract(epoch FROM s.created_at))::bigint)";

/// Failure messages listed per item.
const MAX_ERRORS: usize = 50;

/// Where a backfill is, as the admin queue shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueState {
    /// Not started: the enqueuer has not queued its jobs yet.
    New,
    /// Jobs queued, none started.
    Queued,
    /// Jobs running or done, some still to run.
    Processing,
    Completed,
    Failed,
}

impl QueueState {
    /// Order of the `STATE` sort (the state column sorts as text).
    fn rank(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::New => 2,
            Self::Processing => 3,
            Self::Queued => 4,
        }
    }

    /// Whether the backfill finished (it can be retried, not moved).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// One backfill in the queue: a series on a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// The series (`insight_series.id`).
    pub series_id: i64,
    pub view_id: i64,
    /// The view's public identifier.
    pub view_unique_id: String,
    pub view_title: String,
    /// Who created the view.
    pub creator: Option<UserId>,
    /// The series' label on the view.
    pub label: String,
    pub query: String,
    pub state: QueueState,
    /// 1-based rank among the unfinished backfills.
    pub queue_position: Option<i64>,
    /// Why the series cannot be counted, then the failure messages of its jobs.
    pub errors: Vec<String>,
    /// Estimated cost: repositories × points to count.
    pub cost: Option<i64>,
    pub percent_complete: Option<i32>,
    /// When the series (and so its backfill) was created.
    pub created_at: DateTime<Utc>,
    /// When the first job started (else when the backfill was queued, once it finished).
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl QueueItem {
    /// Time from the start to the completion of a finished backfill.
    pub fn runtime(&self) -> Option<TimeDelta> {
        Some(self.completed_at? - self.started_at?).filter(|d| *d >= TimeDelta::zero())
    }
}

/// Which items to list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueFilter {
    /// Only these states (all when empty).
    pub states: Vec<QueueState>,
    /// Text the view title or the series label contains (ignoring case).
    pub text: Option<String>,
    /// Only this series.
    pub series_id: Option<i64>,
}

/// How to sort the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueOrder {
    State,
    /// Unfinished backfills by position, then the finished ones.
    QueuePosition,
}

/// Sorts `items` by `order` (then series and view, so pages are stable).
pub fn sort_queue(items: &mut [QueueItem], order: QueueOrder, descending: bool) {
    items.sort_by(|a, b| {
        let primary = match order {
            QueueOrder::State => a.state.rank().cmp(&b.state.rank()),
            QueueOrder::QueuePosition => a
                .queue_position
                .unwrap_or(i64::MAX)
                .cmp(&b.queue_position.unwrap_or(i64::MAX)),
        };
        let o = primary
            .then(a.series_id.cmp(&b.series_id))
            .then(a.view_id.cmp(&b.view_id));
        if descending { o.reverse() } else { o }
    });
}

/// Why a retry or a move did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueChange {
    Done,
    /// No such live series.
    NotFound,
    /// The backfill is in the wrong state for the change (retry: not finished; move:
    /// finished).
    WrongState(QueueState),
}

impl InsightStore {
    /// The backfill queue items matching `filter`, by series and view.
    pub async fn backfill_queue(&self, filter: &QueueFilter) -> Result<Vec<QueueItem>> {
        let text = filter
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase);
        let rows = sqlx::query(&format!(
            "WITH jobs AS (
                 SELECT series_id,
                        count(*) AS total,
                        count(*) FILTER (WHERE state IN ('completed', 'failed', 'canceled')) AS finished,
                        count(*) FILTER (WHERE state <> 'queued' OR started_at IS NOT NULL) AS started,
                        min(started_at) AS started_at,
                        array_agg(DISTINCT failure_message ORDER BY failure_message)
                            FILTER (WHERE state = 'failed' AND failure_message IS NOT NULL)
                            AS errors
                 FROM insight_backfill_jobs
                 WHERE $1::bigint IS NULL OR series_id = $1
                 GROUP BY series_id
             ),
             live AS (
                 SELECT s.id, row_number() OVER (
                     ORDER BY {PRIORITY} DESC, s.backfill_queued_at NULLS LAST, s.id) AS pos
                 FROM insight_series s
                 WHERE s.deleted_at IS NULL AND s.backfill_state IN ('new', 'running')
             )
             SELECT s.id, v.id AS view_id, v.unique_id, v.title, v.created_by, vs.label, s.query,
                    s.backfill_state, s.error, s.repos_total,
                    cardinality(s.backfill_times) AS points, s.created_at,
                    s.backfill_queued_at, s.backfill_completed_at,
                    coalesce(j.total, 0) AS total, coalesce(j.finished, 0) AS finished,
                    coalesce(j.started, 0) AS started, j.started_at,
                    coalesce(j.errors, '{{}}') AS errors, l.pos
             FROM insight_series s
             JOIN insight_view_series vs ON vs.series_id = s.id
             JOIN insight_views v ON v.id = vs.view_id
             LEFT JOIN jobs j ON j.series_id = s.id
             LEFT JOIN live l ON l.id = s.id
             WHERE s.deleted_at IS NULL
               AND ($1::bigint IS NULL OR s.id = $1)
               AND ($2::text IS NULL OR strpos(lower(v.title), $2) > 0
                    OR strpos(lower(vs.label), $2) > 0)
             ORDER BY s.id, v.id"
        ))
        .bind(filter.series_id)
        .bind(text)
        .fetch_all(self.pool())
        .await?;
        let mut items = Vec::with_capacity(rows.len());
        for r in &rows {
            let state: String = r.try_get("backfill_state")?;
            let (total, finished, started): (i64, i64, i64) = (
                r.try_get("total")?,
                r.try_get("finished")?,
                r.try_get("started")?,
            );
            let state = match state.as_str() {
                "running" if started > 0 => QueueState::Processing,
                "running" => QueueState::Queued,
                "completed" => QueueState::Completed,
                "failed" => QueueState::Failed,
                _ => QueueState::New,
            };
            if !filter.states.is_empty() && !filter.states.contains(&state) {
                continue;
            }
            let mut errors: Vec<String> = r
                .try_get::<Option<String>, _>("error")?
                .into_iter()
                .collect();
            errors.extend(r.try_get::<Vec<String>, _>("errors")?);
            errors.truncate(MAX_ERRORS);
            let repos: i32 = r.try_get("repos_total")?;
            let points: i32 = r.try_get("points")?;
            let queued_at: Option<DateTime<Utc>> = r.try_get("backfill_queued_at")?;
            let percent_complete = match state {
                QueueState::Completed => 100,
                _ if total > 0 => i32::try_from(finished * 100 / total).unwrap_or(100),
                _ => 0,
            };
            items.push(QueueItem {
                series_id: r.try_get("id")?,
                view_id: r.try_get("view_id")?,
                view_unique_id: r.try_get("unique_id")?,
                view_title: r.try_get("title")?,
                creator: r.try_get("created_by")?,
                label: r.try_get("label")?,
                query: r.try_get("query")?,
                state,
                queue_position: r.try_get("pos")?,
                errors,
                cost: (state != QueueState::New)
                    .then(|| i64::from(repos) * i64::from(points.max(1))),
                percent_complete: Some(percent_complete),
                created_at: r.try_get("created_at")?,
                started_at: r
                    .try_get::<Option<DateTime<Utc>>, _>("started_at")?
                    .or(queued_at.filter(|_| state.is_terminal())),
                completed_at: r.try_get("backfill_completed_at")?,
            });
        }
        Ok(items)
    }

    /// The queue state of live series `id`, `None` if there is none.
    async fn queue_state(&self, id: i64) -> Result<Option<QueueState>> {
        let state: Option<(String, bool)> = sqlx::query_as(
            "SELECT s.backfill_state, EXISTS (
                 SELECT 1 FROM insight_backfill_jobs j
                 WHERE j.series_id = s.id AND (j.state <> 'queued' OR j.started_at IS NOT NULL))
             FROM insight_series s WHERE s.id = $1 AND s.deleted_at IS NULL",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(state.map(|(s, started)| match s.as_str() {
            "running" if started => QueueState::Processing,
            "running" => QueueState::Queued,
            "completed" => QueueState::Completed,
            "failed" => QueueState::Failed,
            _ => QueueState::New,
        }))
    }

    /// Restarts the finished backfill of series `id`: the enqueuer plans it again (resolving
    /// the repositories and the query anew) and queues one job per repository. Points are
    /// kept; the new backfill replaces those at its times.
    pub async fn retry_backfill(&self, id: i64) -> Result<QueueChange> {
        let mut tx = self.pool().begin().await?;
        let n = sqlx::query(
            "UPDATE insight_series SET backfill_state = 'new', error = NULL,
                 backfill_queued_at = NULL, backfill_completed_at = NULL,
                 repos_total = 0, repos_done = 0, repos_failed = 0
             WHERE id = $1 AND deleted_at IS NULL AND backfill_state IN ('completed', 'failed')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            tx.rollback().await?;
            return Ok(match self.queue_state(id).await? {
                Some(s) => QueueChange::WrongState(s),
                None => QueueChange::NotFound,
            });
        }
        sqlx::query("DELETE FROM insight_backfill_jobs WHERE series_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        notify(&mut *tx, CHANNEL, &id.to_string()).await?;
        tx.commit().await?;
        Ok(QueueChange::Done)
    }

    /// Moves the unfinished backfill of series `id` to the front (or back) of the queue: a
    /// priority above (below) every other unfinished backfill, for the series and its jobs.
    pub async fn move_backfill(&self, id: i64, front: bool) -> Result<QueueChange> {
        let mut tx = self.pool().begin().await?;
        // One move at a time, so two fronts do not end up with the same priority.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('insight_backfill_priority'))")
            .execute(&mut *tx)
            .await?;
        let (pick, step) = if front { ("max", 1) } else { ("min", -1) };
        let priority: Option<i64> = sqlx::query_scalar(&format!(
            "WITH others AS (
                 SELECT {pick}({PRIORITY}) + $2 AS p FROM insight_series s
                 WHERE s.deleted_at IS NULL AND s.backfill_state IN ('new', 'running')
                   AND s.id <> $1
             )
             UPDATE insight_series s
             SET backfill_priority = CASE WHEN $2 > 0
                     THEN greatest({PRIORITY}, coalesce((SELECT p FROM others), {PRIORITY}))
                     ELSE least({PRIORITY}, coalesce((SELECT p FROM others), {PRIORITY})) END
             WHERE s.id = $1 AND s.deleted_at IS NULL AND s.backfill_state IN ('new', 'running')
             RETURNING s.backfill_priority"
        ))
        .bind(id)
        .bind(step)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(priority) = priority else {
            tx.rollback().await?;
            return Ok(match self.queue_state(id).await? {
                Some(s) => QueueChange::WrongState(s),
                None => QueueChange::NotFound,
            });
        };
        sqlx::query("UPDATE insight_backfill_jobs SET priority = $2 WHERE series_id = $1")
            .bind(id)
            .bind(priority)
            .execute(&mut *tx)
            .await?;
        sourcegull_pgqueue::notify_enqueued(&mut *tx, BACKFILL_JOBS).await?;
        tx.commit().await?;
        Ok(QueueChange::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(series_id: i64, state: QueueState, pos: Option<i64>) -> QueueItem {
        QueueItem {
            series_id,
            view_id: 1,
            view_unique_id: String::new(),
            view_title: String::new(),
            creator: None,
            label: String::new(),
            query: String::new(),
            state,
            queue_position: pos,
            errors: Vec::new(),
            cost: None,
            percent_complete: None,
            created_at: DateTime::default(),
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn queue_sorts_by_state_or_position() {
        let mut items = vec![
            item(1, QueueState::Queued, Some(2)),
            item(2, QueueState::Completed, None),
            item(3, QueueState::Processing, Some(1)),
            item(4, QueueState::New, Some(3)),
        ];
        sort_queue(&mut items, QueueOrder::QueuePosition, false);
        let ids = |v: &[QueueItem]| v.iter().map(|i| i.series_id).collect::<Vec<_>>();
        assert_eq!(ids(&items), vec![3, 1, 4, 2]);
        sort_queue(&mut items, QueueOrder::State, false);
        assert_eq!(ids(&items), vec![2, 4, 3, 1]);
        sort_queue(&mut items, QueueOrder::State, true);
        assert_eq!(ids(&items), vec![1, 3, 4, 2]);
    }

    #[test]
    fn runtime_of_a_finished_backfill() {
        let mut i = item(1, QueueState::Completed, None);
        assert_eq!(i.runtime(), None);
        i.started_at = Some(DateTime::default());
        i.completed_at = Some(DateTime::default() + TimeDelta::seconds(90));
        assert_eq!(i.runtime(), Some(TimeDelta::seconds(90)));
    }
}
