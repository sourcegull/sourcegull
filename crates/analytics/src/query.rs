//! Admin analytics over the rollups: time series by day or week, DAU/WAU/MAU, usage frequency, code
//! navigation by language and repository, bulk changes from the changesets table, and per-user
//! activity. Site admins only (the GraphQL layer checks).

use std::collections::HashMap;

use chrono::{DateTime, Days, Months, NaiveDate, Utc};
use sourcegull_db::{Db, DbError, sqlx};

use crate::rollup::Period;

/// A predefined date range, ending today (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateRange {
    LastThreeMonths,
    LastMonth,
    LastWeek,
}

impl DateRange {
    /// The first day of the range ending on `today`.
    pub fn from(self, today: NaiveDate) -> NaiveDate {
        match self {
            DateRange::LastThreeMonths => today - Months::new(3),
            DateRange::LastMonth => today - Months::new(1),
            DateRange::LastWeek => today - Days::new(7),
        }
    }
}

/// How a time series is bucketed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Daily,
    Weekly,
}

impl Grouping {
    fn period(self) -> Period {
        match self {
            Grouping::Daily => Period::Day,
            Grouping::Weekly => Period::Week,
        }
    }
}

/// The days and grouping of a time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// First day (inclusive).
    pub from: NaiveDate,
    /// Last day (inclusive).
    pub to: NaiveDate,
    pub grouping: Grouping,
}

impl Window {
    /// The window of `range` ending `today`.
    pub fn new(range: DateRange, grouping: Grouping, today: NaiveDate) -> Self {
        Self {
            from: range.from(today),
            to: today,
            grouping,
        }
    }

    /// The bucket starts from the bucket of `from` to the bucket of `to`.
    pub fn buckets(&self) -> Vec<NaiveDate> {
        let p = self.grouping.period();
        let mut out = Vec::new();
        let mut b = p.start(self.from);
        while b <= self.to {
            out.push(b);
            b = p.end(b);
        }
        out
    }
}

/// One bucket of a time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatNode {
    /// First day of the bucket.
    pub date: NaiveDate,
    /// Events.
    pub count: i64,
    /// Distinct users, including anonymous visitors and deleted users.
    pub unique_users: i64,
    /// Distinct users that still exist.
    pub registered_users: i64,
}

/// Totals over the whole window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatSummary {
    pub total_count: i64,
    pub total_unique_users: i64,
    pub total_registered_users: i64,
}

/// A time series with its totals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatItem {
    pub nodes: Vec<StatNode>,
    pub summary: StatSummary,
}

/// Fills the buckets of `window` without rows with zeros.
pub fn fill(window: &Window, rows: &[StatNode]) -> Vec<StatNode> {
    let by_date: HashMap<NaiveDate, StatNode> = rows.iter().map(|r| (r.date, *r)).collect();
    window
        .buckets()
        .into_iter()
        .map(|date| {
            by_date.get(&date).copied().unwrap_or(StatNode {
                date,
                count: 0,
                unique_users: 0,
                registered_users: 0,
            })
        })
        .collect()
}

/// Which sources of events count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SourceFilter {
    #[default]
    Any,
    Only(String),
    Except(String),
}

/// Which events count: by name (empty: all) and source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub names: Vec<String>,
    pub source: SourceFilter,
}

impl EventFilter {
    /// Events with one of `names`, from any source.
    pub fn names<S: AsRef<str>>(names: &[S]) -> Self {
        Self {
            names: names.iter().map(|n| n.as_ref().to_owned()).collect(),
            source: SourceFilter::Any,
        }
    }

    /// Every event.
    pub fn all() -> Self {
        Self::default()
    }

    /// Only events of `source`.
    pub fn only_source(mut self, source: &str) -> Self {
        self.source = SourceFilter::Only(source.to_owned());
        self
    }

    /// Events of any source but `source`.
    pub fn except_source(mut self, source: &str) -> Self {
        self.source = SourceFilter::Except(source.to_owned());
        self
    }

    fn sources(&self) -> (Option<&str>, Option<&str>) {
        match &self.source {
            SourceFilter::Any => (None, None),
            SourceFilter::Only(s) => (Some(s), None),
            SourceFilter::Except(s) => (None, Some(s)),
        }
    }
}

const EVENTS_WHERE: &str = "d.day >= $1 AND d.day <= $2
      AND (cardinality($3::text[]) = 0 OR d.name = ANY($3))
      AND ($4::text IS NULL OR d.source = $4)
      AND ($5::text IS NULL OR d.source <> $5)";

/// The time series of events matching `filter` in `window`.
pub async fn events(db: &Db, filter: &EventFilter, window: &Window) -> Result<StatItem, DbError> {
    let (only, except) = filter.sources();
    let weekly = window.grouping == Grouping::Weekly;
    let rows: Vec<(NaiveDate, i64, i64, i64)> = sqlx::query_as(&format!(
        "SELECT CASE WHEN $6 THEN date_trunc('week', d.day)::date ELSE d.day END AS bucket,
                coalesce(sum(d.count), 0)::bigint,
                count(DISTINCT coalesce('u' || d.user_id::text, 'a' || d.anonymous_user_id)),
                count(DISTINCT u.id)
         FROM event_logs_daily d
         LEFT JOIN users u ON u.id = d.user_id AND u.deleted_at IS NULL
         WHERE {EVENTS_WHERE}
         GROUP BY bucket ORDER BY bucket"
    ))
    .bind(window.from)
    .bind(window.to)
    .bind(&filter.names)
    .bind(only)
    .bind(except)
    .bind(weekly)
    .fetch_all(db.pool())
    .await?;
    let (total_count, total_unique_users, total_registered_users): (i64, i64, i64) =
        sqlx::query_as(&format!(
            "SELECT coalesce(sum(d.count), 0)::bigint,
                    count(DISTINCT coalesce('u' || d.user_id::text, 'a' || d.anonymous_user_id)),
                    count(DISTINCT u.id)
             FROM event_logs_daily d
             LEFT JOIN users u ON u.id = d.user_id AND u.deleted_at IS NULL
             WHERE {EVENTS_WHERE}"
        ))
        .bind(window.from)
        .bind(window.to)
        .bind(&filter.names)
        .bind(only)
        .bind(except)
        .fetch_one(db.pool())
        .await?;
    let rows: Vec<StatNode> = rows
        .into_iter()
        .map(|(date, count, unique_users, registered_users)| StatNode {
            date,
            count,
            unique_users,
            registered_users,
        })
        .collect();
    Ok(StatItem {
        nodes: fill(window, &rows),
        summary: StatSummary {
            total_count,
            total_unique_users,
            total_registered_users,
        },
    })
}

/// A count series from a table with a timestamp (no users).
async fn table_series(
    db: &Db,
    window: &Window,
    at: &str,
    from_where: &str,
) -> Result<StatItem, DbError> {
    let rows: Vec<(NaiveDate, i64)> = sqlx::query_as(&format!(
        "SELECT CASE WHEN $3 THEN date_trunc('week', ({at}) AT TIME ZONE 'UTC')::date
                     ELSE (({at}) AT TIME ZONE 'UTC')::date END AS bucket,
                count(*)
         {from_where} AND ({at}) >= $1 AND ({at}) < $2
         GROUP BY bucket ORDER BY bucket"
    ))
    .bind(crate::rollup::day_start(window.from))
    .bind(crate::rollup::day_start(window.to + Days::new(1)))
    .bind(window.grouping == Grouping::Weekly)
    .fetch_all(db.pool())
    .await?;
    let total = rows.iter().map(|r| r.1).sum();
    let rows: Vec<StatNode> = rows
        .into_iter()
        .map(|(date, count)| StatNode {
            date,
            count,
            unique_users: 0,
            registered_users: 0,
        })
        .collect();
    Ok(StatItem {
        nodes: fill(window, &rows),
        summary: StatSummary {
            total_count: total,
            ..StatSummary::default()
        },
    })
}

/// Changesets that bulk changes published, by creation date.
pub async fn changesets_created(db: &Db, window: &Window) -> Result<StatItem, DbError> {
    table_series(
        db,
        window,
        "created_at",
        "FROM changesets WHERE owned_by_batch_change_id IS NOT NULL
             AND publication_state = 'PUBLISHED'",
    )
    .await
}

/// Changesets of bulk changes merged, by the code host's last update.
pub async fn changesets_merged(db: &Db, window: &Window) -> Result<StatItem, DbError> {
    table_series(
        db,
        window,
        "coalesce(external_updated_at, updated_at)",
        "FROM changesets WHERE external_state = 'MERGED' AND batch_change_ids <> '{}'::jsonb",
    )
    .await
}

/// Active users of one period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivePeriod {
    pub start: NaiveDate,
    pub users: i64,
    pub registered_users: i64,
    pub anonymous_users: i64,
}

/// Active users of the last `count` periods up to the one containing `today`, newest first
/// (zeros for periods without activity).
pub async fn active_users(
    db: &Db,
    period: Period,
    count: usize,
    today: NaiveDate,
) -> Result<Vec<ActivePeriod>, DbError> {
    let mut starts = Vec::with_capacity(count);
    let mut start = period.start(today);
    for _ in 0..count {
        starts.push(start);
        start = period.start(start - Days::new(1));
    }
    let Some(&oldest) = starts.last() else {
        return Ok(Vec::new());
    };
    let rows: Vec<(NaiveDate, i32, i32, i32)> = sqlx::query_as(
        "SELECT period_start, users, registered_users, anonymous_users
         FROM event_logs_active_users WHERE period = $1 AND period_start >= $2",
    )
    .bind(period.as_str())
    .bind(oldest)
    .fetch_all(db.pool())
    .await?;
    let by_start: HashMap<NaiveDate, (i32, i32, i32)> = rows
        .into_iter()
        .map(|(s, u, r, a)| (s, (u, r, a)))
        .collect();
    Ok(starts
        .into_iter()
        .map(|start| {
            let (u, r, a) = by_start.get(&start).copied().unwrap_or_default();
            ActivePeriod {
                start,
                users: i64::from(u),
                registered_users: i64::from(r),
                anonymous_users: i64::from(a),
            }
        })
        .collect())
}

/// How many users used the instance on exactly `days_used` days of a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frequency {
    pub days_used: i64,
    pub users: i64,
    /// Share of all active users in the window, in percent.
    pub percentage: f64,
}

/// Users by number of days used in `window`.
pub async fn frequencies(db: &Db, window: &Window) -> Result<Vec<Frequency>, DbError> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT days_used, count(*) FROM (
             SELECT count(DISTINCT day) AS days_used
             FROM event_logs_daily
             WHERE day >= $1 AND day <= $2
             GROUP BY coalesce('u' || user_id::text, 'a' || anonymous_user_id)
         ) s GROUP BY 1 ORDER BY 1",
    )
    .bind(window.from)
    .bind(window.to)
    .fetch_all(db.pool())
    .await?;
    Ok(with_percentages(&rows))
}

/// Adds the share of each count.
pub fn with_percentages(rows: &[(i64, i64)]) -> Vec<Frequency> {
    let total: i64 = rows.iter().map(|r| r.1).sum();
    rows.iter()
        .map(|&(days_used, users)| Frequency {
            days_used,
            users,
            percentage: if total == 0 {
                0.0
            } else {
                users as f64 * 100.0 / total as f64
            },
        })
        .collect()
}

/// Searches by type in a window, most first.
pub async fn searches_by_type(db: &Db, window: &Window) -> Result<Vec<(String, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT search_type, sum(count)::bigint FROM event_logs_search_daily
         WHERE day >= $1 AND day <= $2 GROUP BY 1 ORDER BY 2 DESC, 1",
    )
    .bind(window.from)
    .bind(window.to)
    .fetch_all(db.pool())
    .await?)
}

/// The search latency histogram of a window: `(bucket upper bound ms, count)`.
pub async fn search_latency(db: &Db, window: &Window) -> Result<Vec<(i32, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT latency_bucket_ms, sum(count)::bigint FROM event_logs_search_daily
         WHERE day >= $1 AND day <= $2 GROUP BY 1 ORDER BY 1",
    )
    .bind(window.from)
    .bind(window.to)
    .fetch_all(db.pool())
    .await?)
}

/// Code navigation events by language and precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageCount {
    pub language: String,
    pub precise: bool,
    pub count: i64,
}

/// Code navigation events by language and precision in a window, most first.
pub async fn codeintel_by_language(
    db: &Db,
    window: &Window,
) -> Result<Vec<LanguageCount>, DbError> {
    let rows: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT language, name LIKE 'codeintel.lsif%', sum(count)::bigint
         FROM event_logs_codeintel_daily
         WHERE day >= $1 AND day <= $2 GROUP BY 1, 2 ORDER BY 3 DESC, 1, 2 DESC",
    )
    .bind(window.from)
    .bind(window.to)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(language, precise, count)| LanguageCount {
            language,
            precise,
            count,
        })
        .collect())
}

/// Code navigation events of one repository, language, kind and precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryCount {
    pub repo_name: String,
    pub language: String,
    /// `hover`, `definitions` or `references`.
    pub kind: String,
    pub precise: bool,
    pub events: i64,
}

/// The rows of the repositories with the most code navigation events in a window (at most
/// `limit` repositories), most first.
pub async fn codeintel_top_repositories(
    db: &Db,
    window: &Window,
    limit: i64,
) -> Result<Vec<RepositoryCount>, DbError> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "WITH top AS (
             SELECT repo_name FROM event_logs_codeintel_daily
             WHERE day >= $1 AND day <= $2 AND repo_name <> ''
             GROUP BY 1 ORDER BY sum(count) DESC, 1 LIMIT $3)
         SELECT c.repo_name, c.language, c.name, sum(c.count)::bigint
         FROM event_logs_codeintel_daily c JOIN top USING (repo_name)
         WHERE c.day >= $1 AND c.day <= $2
         GROUP BY 1, 2, 3",
    )
    .bind(window.from)
    .bind(window.to)
    .bind(limit)
    .fetch_all(db.pool())
    .await?;
    let mut merged: HashMap<(String, String, &'static str, bool), i64> = HashMap::new();
    for (repo_name, language, name, events) in rows {
        let Some(action) = crate::names::codeintel_action(&name) else {
            continue;
        };
        *merged
            .entry((
                repo_name,
                language,
                action.kind(),
                crate::names::is_precise(&name),
            ))
            .or_default() += events;
    }
    let mut out: Vec<RepositoryCount> = merged
        .into_iter()
        .map(
            |((repo_name, language, kind, precise), events)| RepositoryCount {
                repo_name,
                language,
                kind: kind.to_owned(),
                precise,
                events,
            },
        )
        .collect();
    out.sort_by(|a, b| {
        b.events
            .cmp(&a.events)
            .then_with(|| a.repo_name.cmp(&b.repo_name))
            .then_with(|| a.language.cmp(&b.language))
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| b.precise.cmp(&a.precise))
    });
    Ok(out)
}

/// Which of `repos` (names) have a completed precise index.
pub async fn repos_with_precise(db: &Db, repos: &[String]) -> Result<Vec<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT r.name FROM repo r
         WHERE r.deleted_at IS NULL AND r.name = ANY($1) AND EXISTS (
             SELECT 1 FROM codeintel_uploads u
             WHERE u.repository_id = r.id AND u.state = 'completed')",
    )
    .bind(repos)
    .fetch_all(db.pool())
    .await?)
}

/// Repositories, and those with a completed precise index.
pub async fn repos_summary(db: &Db) -> Result<(i64, i64), DbError> {
    Ok(sqlx::query_as(
        "SELECT count(*),
                count(*) FILTER (WHERE EXISTS (
                    SELECT 1 FROM codeintel_uploads u
                    WHERE u.repository_id = r.id AND u.state = 'completed'))
         FROM repo r WHERE r.deleted_at IS NULL",
    )
    .fetch_one(db.pool())
    .await?)
}

/// When the rollups were last updated.
pub async fn last_rollup(db: &Db) -> Result<Option<DateTime<Utc>>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT max(rolled_up_at) FROM event_logs_rollup_days")
            .fetch_one(db.pool())
            .await?,
    )
}

/// How the user list is sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserOrder {
    #[default]
    Username,
    Email,
    EventsCount,
    LastActiveAt,
    CreatedAt,
    DeletedAt,
    SiteAdmin,
}

impl UserOrder {
    fn column(self) -> &'static str {
        match self {
            UserOrder::Username => "lower(username)",
            UserOrder::Email => "lower(email)",
            UserOrder::EventsCount => "events_count",
            UserOrder::LastActiveAt => "last_active_at",
            UserOrder::CreatedAt => "created_at",
            UserOrder::DeletedAt => "deleted_at",
            UserOrder::SiteAdmin => "site_admin",
        }
    }
}

/// A timestamp range filter (`SiteUsersDateRangeInput`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeRange {
    pub gte: Option<DateTime<Utc>>,
    pub lte: Option<DateTime<Utc>>,
    /// Only null values (`Some(true)`) or only non-null ones (`Some(false)`).
    pub empty: Option<bool>,
    /// Negates the whole filter.
    pub not: bool,
}

/// Filters and order of the user list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserListOptions {
    /// Matches username, display name or email (case-insensitive substring).
    pub query: Option<String>,
    pub site_admin: Option<bool>,
    pub last_active_at: Option<TimeRange>,
    pub created_at: Option<TimeRange>,
    pub deleted_at: Option<TimeRange>,
    pub events_count_gte: Option<f64>,
    pub events_count_lte: Option<f64>,
    pub order: UserOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

/// A user with activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserActivity {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub site_admin: bool,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// Locked out of password sign-in from at least one client address (`auth.lockout`).
    pub locked: bool,
    pub scim_controlled: bool,
    pub last_active_at: Option<DateTime<Utc>>,
    /// Events in the rollups (within `eventLogs.rollupRetention`).
    pub events_count: i64,
}

const USERS_CTE: &str = "WITH s AS (
    SELECT u.id, u.username, u.display_name, u.site_admin, u.created_at, u.deleted_at,
           EXISTS (SELECT 1 FROM sign_in_failures f WHERE f.user_id = u.id
                   AND f.locked_until > now()) AS locked,
           (SELECT e.email FROM user_emails e WHERE e.user_id = u.id
            ORDER BY e.is_primary DESC, e.created_at LIMIT 1) AS email,
           EXISTS (SELECT 1 FROM user_external_accounts a WHERE a.user_id = u.id
                   AND a.service_type = 'scim' AND a.service_id = 'scim'
                   AND a.deleted_at IS NULL) AS scim_controlled,
           coalesce((SELECT max(l.timestamp) FROM event_logs l WHERE l.user_id = u.id),
                    (SELECT max(d.day) FROM event_logs_daily d WHERE d.user_id = u.id)
                        ::timestamp AT TIME ZONE 'UTC') AS last_active_at,
           coalesce((SELECT sum(d.count) FROM event_logs_daily d WHERE d.user_id = u.id), 0)
               ::bigint AS events_count
    FROM users u)";

fn range_sql(column: &str, first: usize) -> String {
    // $first: gte, +1: lte, +2: empty, +3: not, +4: whether the filter is set.
    let (gte, lte, empty, not, set) = (first, first + 1, first + 2, first + 3, first + 4);
    format!(
        "(NOT ${set}::boolean OR (${not}::boolean <> coalesce(
             (${gte}::timestamptz IS NULL OR {column} >= ${gte})
             AND (${lte}::timestamptz IS NULL OR {column} <= ${lte})
             AND (${empty}::boolean IS NULL OR ({column} IS NULL) = ${empty}), false)))"
    )
}

fn users_where() -> String {
    format!(
        "WHERE ($1::text IS NULL OR s.username ILIKE $1 OR s.display_name ILIKE $1
                OR s.email ILIKE $1)
           AND ($2::boolean IS NULL OR s.site_admin = $2)
           AND ($3::float8 IS NULL OR s.events_count >= $3)
           AND ($4::float8 IS NULL OR s.events_count <= $4)
           AND {} AND {} AND {}",
        range_sql("s.last_active_at", 5),
        range_sql("s.created_at", 10),
        range_sql("s.deleted_at", 15),
    )
}

type RangeBinds = (
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<bool>,
    bool,
    bool,
);

fn range_binds(r: Option<TimeRange>) -> RangeBinds {
    match r {
        Some(r) => (r.gte, r.lte, r.empty, r.not, true),
        None => (None, None, None, false, false),
    }
}

fn like_pattern(q: &str) -> String {
    let mut out = String::from("%");
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

macro_rules! bind_user_filters {
    ($q:expr, $opts:expr) => {{
        let o = $opts;
        let pattern = o
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(like_pattern);
        let mut q = $q
            .bind(pattern)
            .bind(o.site_admin)
            .bind(o.events_count_gte)
            .bind(o.events_count_lte);
        for r in [o.last_active_at, o.created_at, o.deleted_at] {
            let (gte, lte, empty, not, set) = range_binds(r);
            q = q.bind(gte).bind(lte).bind(empty).bind(not).bind(set);
        }
        q
    }};
}

/// Users with their activity, filtered and sorted.
pub async fn list_users(db: &Db, opts: &UserListOptions) -> Result<Vec<UserActivity>, DbError> {
    let sql = format!(
        "{USERS_CTE}
         SELECT id, username, display_name, email, site_admin, created_at, deleted_at, locked,
                scim_controlled, last_active_at, events_count
         FROM s {}
         ORDER BY {} {} NULLS LAST, id
         LIMIT $20 OFFSET $21",
        users_where(),
        opts.order.column(),
        if opts.descending { "DESC" } else { "ASC" },
    );
    type Row = (
        i64,
        String,
        Option<String>,
        Option<String>,
        bool,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        bool,
        bool,
        Option<DateTime<Utc>>,
        i64,
    );
    let q = sqlx::query_as::<_, Row>(&sql);
    let rows = bind_user_filters!(q, opts)
        .bind(opts.limit)
        .bind(opts.offset)
        .fetch_all(db.pool())
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                username,
                display_name,
                email,
                site_admin,
                created_at,
                deleted_at,
                locked,
                scim_controlled,
                last_active_at,
                events_count,
            )| UserActivity {
                id,
                username,
                display_name,
                email,
                site_admin,
                created_at,
                deleted_at,
                locked,
                scim_controlled,
                last_active_at,
                events_count,
            },
        )
        .collect())
}

/// The number of users [`list_users`] would list without paging.
pub async fn count_users(db: &Db, opts: &UserListOptions) -> Result<i64, DbError> {
    let sql = format!("{USERS_CTE} SELECT count(*) FROM s {}", users_where());
    let q = sqlx::query_scalar::<_, i64>(&sql);
    Ok(bind_user_filters!(q, opts).fetch_one(db.pool()).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn ranges() {
        let today = d("2026-10-21");
        assert_eq!(DateRange::LastWeek.from(today), d("2026-10-14"));
        assert_eq!(DateRange::LastMonth.from(today), d("2026-09-21"));
        assert_eq!(DateRange::LastThreeMonths.from(today), d("2026-07-21"));
    }

    #[test]
    fn buckets_and_fill() {
        let w = Window {
            from: d("2026-10-14"),
            to: d("2026-10-21"),
            grouping: Grouping::Daily,
        };
        assert_eq!(w.buckets().len(), 8);
        let weekly = Window {
            grouping: Grouping::Weekly,
            ..w
        };
        // 2026-10-14 is a Wednesday: weeks of 10-12 and 10-19.
        assert_eq!(weekly.buckets(), vec![d("2026-10-12"), d("2026-10-19")]);
        let rows = [StatNode {
            date: d("2026-10-19"),
            count: 5,
            unique_users: 2,
            registered_users: 1,
        }];
        let filled = fill(&weekly, &rows);
        assert_eq!(filled.len(), 2);
        assert_eq!(filled[0].count, 0);
        assert_eq!(filled[0].date, d("2026-10-12"));
        assert_eq!(filled[1], rows[0]);
    }

    #[test]
    fn percentages() {
        let f = with_percentages(&[(1, 3), (2, 1)]);
        assert_eq!(f[0].percentage, 75.0);
        assert_eq!(f[1].percentage, 25.0);
        assert!(with_percentages(&[]).is_empty());
    }

    #[test]
    fn like_patterns_escape() {
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
    }

    #[test]
    fn range_sql_numbers_parameters() {
        let s = range_sql("x", 5);
        for n in 5..=9 {
            assert!(s.contains(&format!("${n}")), "{s}");
        }
        assert!(!users_where().contains("$20"));
    }
}
