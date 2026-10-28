//! Trends and dashboards (migration `20261117000001_insight_views`).
//!
//! A view is a chart: a line chart over one or more series (each with a label and a color),
//! or a pie chart of language statistics. Series are shared: a view that defines a series
//! identical to an existing one (same query, generation method, group, repository scope and
//! interval) reuses it, and a series no view uses any more is deleted. Views carry their
//! default filters and series display options.
//!
//! Visibility follows grants: a view is visible to the users, organizations'
//! members or everyone it is granted to (its creator at first), and to whoever sees a
//! dashboard it is on. Dashboards are visible through their own grants. Whoever sees a view
//! or a dashboard may change it (there is no viewer/editor split).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sourcegull_db::Result;
use sourcegull_types::{OrgId, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::store::{InsightStore, NewSeries, Series, series_from_row};

/// Node kind of view IDs: `base64("insight_view:\"<unique id>\"")`.
pub const VIEW_KIND: &str = "insight_view";

/// The node ID of the view with this unique ID.
pub fn view_node_id(unique_id: &str) -> String {
    sourcegull_services::relay::marshal_string_id(VIEW_KIND, unique_id)
}

/// The unique ID in a view node ID.
pub fn parse_view_node_id(id: &str) -> Option<String> {
    let (kind, spec) = sourcegull_services::relay::unmarshal_id(id)?;
    if kind != VIEW_KIND {
        return None;
    }
    serde_json::from_str::<String>(&spec).ok()
}

/// How a view is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Presentation {
    Line,
    Pie,
}

impl Presentation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Line => "LINE",
            Self::Pie => "PIE",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "PIE" { Self::Pie } else { Self::Line }
    }
}

/// How a view's series are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortMode {
    /// By the newest point's value.
    ResultCount,
    /// By label (semantic versions compared as versions).
    Lexicographical,
    /// By the oldest point's time.
    DateAdded,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResultCount => "RESULT_COUNT",
            Self::Lexicographical => "LEXICOGRAPHICAL",
            Self::DateAdded => "DATE_ADDED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "RESULT_COUNT" => Self::ResultCount,
            "LEXICOGRAPHICAL" => Self::Lexicographical,
            "DATE_ADDED" => Self::DateAdded,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ASC" => Self::Asc,
            "DESC" => Self::Desc,
            _ => return None,
        })
    }
}

/// Which repositories a view's points count (applied when points are read).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewFilters {
    /// Only repositories whose name matches.
    pub include_repo_regex: Option<String>,
    /// No repository whose name matches.
    pub exclude_repo_regex: Option<String>,
    /// Only repositories in one of these search contexts.
    pub search_contexts: Vec<String>,
}

impl ViewFilters {
    pub fn is_empty(&self) -> bool {
        self.include_repo_regex.as_deref().is_none_or(str::is_empty)
            && self.exclude_repo_regex.as_deref().is_none_or(str::is_empty)
            && self.search_contexts.is_empty()
    }
}

/// How a view's series are shown: sorted, at most `limit` of them, each with at most
/// `num_samples` points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayOptions {
    pub sort_mode: Option<SortMode>,
    pub sort_direction: Option<SortDirection>,
    pub limit: Option<i32>,
    pub num_samples: Option<i32>,
}

/// One series of a view.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSeries {
    pub series: Series,
    pub label: String,
    /// The line color.
    pub stroke: String,
}

/// A trend with its series in order.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub id: i64,
    pub unique_id: String,
    pub title: String,
    pub presentation: Presentation,
    pub filters: ViewFilters,
    pub display: DisplayOptions,
    pub other_threshold: Option<f64>,
    pub pie_query: Option<String>,
    pub pie_repositories: Vec<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub series: Vec<ViewSeries>,
}

/// A view to create or the new settings of one.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSettings {
    pub title: String,
    pub presentation: Presentation,
    pub filters: ViewFilters,
    pub display: DisplayOptions,
    pub other_threshold: Option<f64>,
    pub pie_query: Option<String>,
    pub pie_repositories: Vec<String>,
}

/// Who a view or a dashboard is granted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grant {
    User(UserId),
    Org(OrgId),
    Global,
}

/// Who is asking: a user and the organizations they belong to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Principal {
    pub user: Option<UserId>,
    pub orgs: Vec<i64>,
}

impl Principal {
    /// Whether any of `grants` reaches this principal.
    pub fn reached_by(&self, grants: &[Grant]) -> bool {
        grants.iter().any(|g| match g {
            Grant::User(u) => self.user == Some(*u),
            Grant::Org(o) => self.orgs.contains(&o.0),
            Grant::Global => true,
        })
    }

    /// Whether this principal may give every one of `grants` (dashboard grants go only to
    /// oneself and one's organizations; global is open to all).
    pub fn may_grant(&self, grants: &[Grant]) -> bool {
        grants.iter().all(|g| match g {
            Grant::User(u) => self.user == Some(*u),
            Grant::Org(o) => self.orgs.contains(&o.0),
            Grant::Global => true,
        })
    }

    fn user_id(&self) -> i64 {
        self.user.map_or(0, |u| u.0)
    }
}

/// A dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dashboard {
    pub id: i64,
    pub title: String,
    pub created_by: Option<UserId>,
    pub grants: Vec<Grant>,
    /// The ids of its views, in the order they were added.
    pub views: Vec<i64>,
}

/// What [`ViewStore::list_views`] selects.
#[derive(Debug, Clone, Default)]
pub struct ViewQuery {
    pub unique_ids: Vec<String>,
    pub exclude_unique_ids: Vec<String>,
    /// In the title or a series label, case-insensitively.
    pub find: Option<String>,
    /// Views after this unique ID (they are ordered by it).
    pub after: Option<String>,
    pub limit: Option<i64>,
}

/// SQL: the dashboards `$1` (user) and `$2` (orgs) may see.
const VISIBLE_DASHBOARDS: &str = "SELECT d.id FROM insight_dashboards d
     WHERE d.deleted_at IS NULL AND EXISTS (
         SELECT 1 FROM insight_dashboard_grants g
         WHERE g.dashboard_id = d.id AND (g.user_id = $1 OR g.org_id = ANY($2) OR g.global))";

/// SQL condition on `v`: the view is visible to `$1` / `$2`.
fn visible_view_condition() -> String {
    format!(
        "(v.id IN (SELECT g.view_id FROM insight_view_grants g
                   WHERE g.user_id = $1 OR g.org_id = ANY($2) OR g.global)
          OR v.id IN (SELECT dv.view_id FROM insight_dashboard_views dv
                      WHERE dv.dashboard_id IN ({VISIBLE_DASHBOARDS})))"
    )
}

/// SQL condition of view listings: visible to `$1` / `$2`, among the unique IDs `$3` (all
/// when empty), not among `$4`, matching `$5` (a LIKE pattern) in the title or a series label.
fn list_condition() -> String {
    format!(
        "{}
           AND (cardinality($3::text[]) = 0 OR v.unique_id = ANY($3))
           AND NOT v.unique_id = ANY($4)
           AND ($5::text IS NULL OR v.title ILIKE $5 OR EXISTS (
               SELECT 1 FROM insight_view_series vs
               WHERE vs.view_id = v.id AND vs.label ILIKE $5))",
        visible_view_condition()
    )
}

/// The LIKE pattern of a `find` text.
fn find_pattern(find: Option<&str>) -> Option<String> {
    find.filter(|f| !f.is_empty()).map(|f| {
        format!(
            "%{}%",
            f.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    })
}

const VIEW_COLUMNS: &str = "v.id, v.unique_id, v.title, v.presentation_type, \
     v.filter_include_repo_regex, v.filter_exclude_repo_regex, v.filter_search_contexts, \
     v.series_sort_mode, v.series_sort_direction, v.series_limit, v.series_num_samples, \
     v.other_threshold, v.pie_query, v.pie_repositories, v.created_by, v.created_at";

fn view_from_row(r: &PgRow) -> sqlx::Result<View> {
    let presentation: String = r.try_get("presentation_type")?;
    let mode: Option<String> = r.try_get("series_sort_mode")?;
    let direction: Option<String> = r.try_get("series_sort_direction")?;
    Ok(View {
        id: r.try_get("id")?,
        unique_id: r.try_get("unique_id")?,
        title: r.try_get("title")?,
        presentation: Presentation::parse(&presentation),
        filters: ViewFilters {
            include_repo_regex: r.try_get("filter_include_repo_regex")?,
            exclude_repo_regex: r.try_get("filter_exclude_repo_regex")?,
            search_contexts: r.try_get("filter_search_contexts")?,
        },
        display: DisplayOptions {
            sort_mode: mode.as_deref().and_then(SortMode::parse),
            sort_direction: direction.as_deref().and_then(SortDirection::parse),
            limit: r.try_get("series_limit")?,
            num_samples: r.try_get("series_num_samples")?,
        },
        other_threshold: r.try_get("other_threshold")?,
        pie_query: r.try_get("pie_query")?,
        pie_repositories: r.try_get("pie_repositories")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        series: Vec::new(),
    })
}

fn grant_from_row(user: Option<i64>, org: Option<i64>, global: bool) -> Option<Grant> {
    match (user, org, global) {
        (Some(u), _, _) => Some(Grant::User(UserId(u))),
        (_, Some(o), _) => Some(Grant::Org(OrgId(o))),
        (_, _, true) => Some(Grant::Global),
        _ => None,
    }
}

fn grant_columns(g: Grant) -> (Option<i64>, Option<i64>, bool) {
    match g {
        Grant::User(u) => (Some(u.0), None, false),
        Grant::Org(o) => (None, Some(o.0), false),
        Grant::Global => (None, None, true),
    }
}

/// The view and dashboard tables.
#[derive(Debug, Clone)]
pub struct ViewStore {
    pool: PgPool,
}

impl ViewStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The principal of `user`: the user and their live organizations.
    pub async fn principal(&self, user: Option<UserId>) -> Result<Principal> {
        let orgs = match user {
            Some(u) => {
                sqlx::query_scalar(
                    "SELECT m.org_id FROM org_members m JOIN orgs o ON o.id = m.org_id
                     WHERE m.user_id = $1 AND o.deleted_at IS NULL ORDER BY m.org_id",
                )
                .bind(u)
                .fetch_all(&self.pool)
                .await?
            }
            None => Vec::new(),
        };
        Ok(Principal { user, orgs })
    }

    // --- views ---

    /// Creates a view granted to `grants`, with no series yet.
    pub async fn create_view_in(
        tx: &mut PgConnection,
        settings: &ViewSettings,
        grants: &[Grant],
        created_by: Option<UserId>,
    ) -> Result<View> {
        let unique_id = uuid::Uuid::new_v4().simple().to_string();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO insight_views (unique_id, title, presentation_type, created_by)
             VALUES ($1, '', $2, $3) RETURNING id",
        )
        .bind(&unique_id)
        .bind(settings.presentation.as_str())
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await?;
        Self::update_view_in(tx, id, settings).await?;
        for g in grants {
            let (user, org, global) = grant_columns(*g);
            sqlx::query(
                "INSERT INTO insight_view_grants (view_id, user_id, org_id, global)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(user)
            .bind(org)
            .bind(global)
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query(&format!(
            "SELECT {VIEW_COLUMNS} FROM insight_views v WHERE v.id = $1"
        ))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        Ok(view_from_row(&row)?)
    }

    /// Replaces a view's settings.
    pub async fn update_view_in(tx: &mut PgConnection, id: i64, s: &ViewSettings) -> Result<()> {
        sqlx::query(
            "UPDATE insight_views SET title = $2, presentation_type = $3,
                 filter_include_repo_regex = $4, filter_exclude_repo_regex = $5,
                 filter_search_contexts = $6, series_sort_mode = $7, series_sort_direction = $8,
                 series_limit = $9, series_num_samples = $10, other_threshold = $11,
                 pie_query = $12, pie_repositories = $13, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(&s.title)
        .bind(s.presentation.as_str())
        .bind(
            s.filters
                .include_repo_regex
                .as_deref()
                .filter(|r| !r.is_empty()),
        )
        .bind(
            s.filters
                .exclude_repo_regex
                .as_deref()
                .filter(|r| !r.is_empty()),
        )
        .bind(&s.filters.search_contexts)
        .bind(s.display.sort_mode.map(SortMode::as_str))
        .bind(s.display.sort_direction.map(SortDirection::as_str))
        .bind(s.display.limit)
        .bind(s.display.num_samples)
        .bind(s.other_threshold)
        .bind(&s.pie_query)
        .bind(&s.pie_repositories)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// The series defined as `new`: an identical live series, or a new one.
    pub async fn series_for_in(tx: &mut PgConnection, new: &NewSeries) -> Result<Series> {
        if let Some(s) = InsightStore::find_matching_in(tx, new).await? {
            return Ok(s);
        }
        InsightStore::create_in(tx, new).await
    }

    /// Puts series `series` on view `view` (or updates its label and color there).
    pub async fn attach_in(
        tx: &mut PgConnection,
        view: i64,
        series: i64,
        label: &str,
        stroke: &str,
        position: i32,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO insight_view_series (view_id, series_id, label, stroke, position)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (view_id, series_id) DO UPDATE SET
                 label = EXCLUDED.label, stroke = EXCLUDED.stroke, position = EXCLUDED.position",
        )
        .bind(view)
        .bind(series)
        .bind(label)
        .bind(stroke)
        .bind(position)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Takes series `series` off view `view`; a series left on no view is deleted.
    pub async fn detach_in(tx: &mut PgConnection, view: i64, series: i64) -> Result<()> {
        sqlx::query("DELETE FROM insight_view_series WHERE view_id = $1 AND series_id = $2")
            .bind(view)
            .bind(series)
            .execute(&mut *tx)
            .await?;
        Self::delete_if_orphan_in(tx, series).await
    }

    async fn delete_if_orphan_in(tx: &mut PgConnection, series: i64) -> Result<()> {
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM insight_view_series WHERE series_id = $1)",
        )
        .bind(series)
        .fetch_one(&mut *tx)
        .await?;
        if !used {
            InsightStore::delete_in(tx, series).await?;
        }
        Ok(())
    }

    /// Deletes view `id`; its series no other view uses are deleted.
    pub async fn delete_view(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let series: Vec<i64> =
            sqlx::query_scalar("SELECT series_id FROM insight_view_series WHERE view_id = $1")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let n = sqlx::query("DELETE FROM insight_views WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        for s in series {
            Self::delete_if_orphan_in(&mut tx, s).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    /// The view with this unique ID (no visibility check).
    pub async fn view_by_unique_id(&self, unique_id: &str) -> Result<Option<View>> {
        let row = sqlx::query(&format!(
            "SELECT {VIEW_COLUMNS} FROM insight_views v WHERE v.unique_id = $1"
        ))
        .bind(unique_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut views = vec![view_from_row(&row)?];
        self.load_series(&mut views).await?;
        Ok(views.pop())
    }

    /// Whether `who` sees view `id`.
    pub async fn view_visible(&self, who: &Principal, id: i64) -> Result<bool> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM insight_views v WHERE v.id = $3 AND {})",
            visible_view_condition()
        ))
        .bind(who.user_id())
        .bind(&who.orgs)
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The views `who` sees that `q` selects, ordered by unique ID, with their series.
    pub async fn list_views(&self, who: &Principal, q: &ViewQuery) -> Result<Vec<View>> {
        let rows = sqlx::query(&format!(
            "SELECT {VIEW_COLUMNS} FROM insight_views v WHERE {}
               AND ($6::text IS NULL OR v.unique_id > $6)
             ORDER BY v.unique_id
             LIMIT $7",
            list_condition()
        ))
        .bind(who.user_id())
        .bind(&who.orgs)
        .bind(&q.unique_ids)
        .bind(&q.exclude_unique_ids)
        .bind(find_pattern(q.find.as_deref()))
        .bind(&q.after)
        .bind(q.limit)
        .fetch_all(&self.pool)
        .await?;
        let mut views = rows
            .iter()
            .map(view_from_row)
            .collect::<sqlx::Result<Vec<_>>>()?;
        self.load_series(&mut views).await?;
        Ok(views)
    }

    /// How many views `who` sees that `q` selects (ignoring `after` and `limit`).
    pub async fn count_views(&self, who: &Principal, q: &ViewQuery) -> Result<i64> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM insight_views v WHERE {}",
            list_condition()
        ))
        .bind(who.user_id())
        .bind(&who.orgs)
        .bind(&q.unique_ids)
        .bind(&q.exclude_unique_ids)
        .bind(find_pattern(q.find.as_deref()))
        .fetch_one(&self.pool)
        .await?)
    }

    /// The views with these ids (no visibility check), in the order given.
    pub async fn views_by_ids(&self, ids: &[i64]) -> Result<Vec<View>> {
        let rows = sqlx::query(&format!(
            "SELECT {VIEW_COLUMNS} FROM insight_views v WHERE v.id = ANY($1)"
        ))
        .bind(ids)
        .fetch_all(&self.pool)
        .await?;
        let mut views = rows
            .iter()
            .map(view_from_row)
            .collect::<sqlx::Result<Vec<_>>>()?;
        self.load_series(&mut views).await?;
        let at: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        views.sort_by_key(|v| at.get(&v.id).copied().unwrap_or(usize::MAX));
        Ok(views)
    }

    async fn load_series(&self, views: &mut [View]) -> Result<()> {
        if views.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = views.iter().map(|v| v.id).collect();
        let rows = sqlx::query(
            "SELECT vs.view_id AS vs_view_id, vs.label AS vs_label, vs.stroke AS vs_stroke, s.*
             FROM insight_view_series vs JOIN insight_series s ON s.id = vs.series_id
             WHERE vs.view_id = ANY($1)
             ORDER BY vs.view_id, vs.position, s.id",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let at: HashMap<i64, usize> = views.iter().enumerate().map(|(i, v)| (v.id, i)).collect();
        for r in &rows {
            let view: i64 = r.try_get("vs_view_id")?;
            let s = ViewSeries {
                series: series_from_row(r)?,
                label: r.try_get("vs_label")?,
                stroke: r.try_get("vs_stroke")?,
            };
            if let Some(&i) = at.get(&view) {
                views[i].series.push(s);
            }
        }
        Ok(())
    }

    /// On how many dashboards view `id` is (regardless of who sees them).
    pub async fn dashboard_reference_count(&self, id: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM insight_dashboard_views dv
             JOIN insight_dashboards d ON d.id = dv.dashboard_id
             WHERE dv.view_id = $1 AND d.deleted_at IS NULL",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    // --- dashboards ---

    pub async fn create_dashboard(
        &self,
        title: &str,
        grants: &[Grant],
        created_by: Option<UserId>,
    ) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO insight_dashboards (title, created_by) VALUES ($1, $2) RETURNING id",
        )
        .bind(title)
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await?;
        Self::set_dashboard_grants_in(&mut tx, id, grants).await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn set_dashboard_grants_in(
        tx: &mut PgConnection,
        id: i64,
        grants: &[Grant],
    ) -> Result<()> {
        sqlx::query("DELETE FROM insight_dashboard_grants WHERE dashboard_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for g in grants {
            let (user, org, global) = grant_columns(*g);
            sqlx::query(
                "INSERT INTO insight_dashboard_grants (dashboard_id, user_id, org_id, global)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(user)
            .bind(org)
            .bind(global)
            .execute(&mut *tx)
            .await?;
        }
        Ok(())
    }

    /// Sets a dashboard's title and, when given, replaces its grants.
    pub async fn update_dashboard(
        &self,
        id: i64,
        title: Option<&str>,
        grants: Option<&[Grant]>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if let Some(t) = title {
            sqlx::query("UPDATE insight_dashboards SET title = $2 WHERE id = $1")
                .bind(id)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(g) = grants {
            Self::set_dashboard_grants_in(&mut tx, id, g).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Deletes a dashboard (softly); its views stay.
    pub async fn delete_dashboard(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE insight_dashboards SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Whether `who` sees dashboard `id` (and so may change it).
    pub async fn dashboard_visible(&self, who: &Principal, id: i64) -> Result<bool> {
        Ok(
            sqlx::query_scalar(&format!("SELECT $3 IN ({VISIBLE_DASHBOARDS})"))
                .bind(who.user_id())
                .bind(&who.orgs)
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// The dashboards `who` sees, ordered by id: all of them, those in `ids`, or those
    /// holding view `with_view`, after id `after`, at most `limit`.
    pub async fn list_dashboards(
        &self,
        who: &Principal,
        ids: &[i64],
        with_view: Option<i64>,
        after: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<Dashboard>> {
        let rows: Vec<(i64, String, Option<i64>)> = sqlx::query_as(&format!(
            "SELECT d.id, d.title, d.created_by FROM insight_dashboards d
             WHERE d.id IN ({VISIBLE_DASHBOARDS})
               AND (cardinality($3::bigint[]) = 0 OR d.id = ANY($3))
               AND ($4::bigint IS NULL OR d.id IN (
                   SELECT dashboard_id FROM insight_dashboard_views WHERE view_id = $4))
               AND ($5::bigint IS NULL OR d.id > $5)
             ORDER BY d.id
             LIMIT $6"
        ))
        .bind(who.user_id())
        .bind(&who.orgs)
        .bind(ids)
        .bind(with_view)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        self.complete_dashboards(rows).await
    }

    /// Dashboard `id` unless deleted, whoever sees it.
    pub async fn dashboard_by_id(&self, id: i64) -> Result<Option<Dashboard>> {
        let rows: Vec<(i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT id, title, created_by FROM insight_dashboards
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(self.complete_dashboards(rows).await?.pop())
    }

    async fn complete_dashboards(
        &self,
        rows: Vec<(i64, String, Option<i64>)>,
    ) -> Result<Vec<Dashboard>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
        let grants: Vec<(i64, Option<i64>, Option<i64>, bool)> = sqlx::query_as(
            "SELECT dashboard_id, user_id, org_id, global FROM insight_dashboard_grants
             WHERE dashboard_id = ANY($1) ORDER BY id",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let views: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT dashboard_id, view_id FROM insight_dashboard_views
             WHERE dashboard_id = ANY($1) ORDER BY id",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<Dashboard> = rows
            .into_iter()
            .map(|(id, title, created_by)| Dashboard {
                id,
                title,
                created_by: created_by.map(UserId),
                grants: Vec::new(),
                views: Vec::new(),
            })
            .collect();
        let at: HashMap<i64, usize> = out.iter().enumerate().map(|(i, d)| (d.id, i)).collect();
        for (d, user, org, global) in grants {
            if let (Some(&i), Some(g)) = (at.get(&d), grant_from_row(user, org, global)) {
                out[i].grants.push(g);
            }
        }
        for (d, v) in views {
            if let Some(&i) = at.get(&d) {
                out[i].views.push(v);
            }
        }
        Ok(out)
    }

    /// Adds view `view` to dashboard `dashboard` (once).
    pub async fn add_view_to_dashboard(&self, dashboard: i64, view: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO insight_dashboard_views (dashboard_id, view_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(dashboard)
        .bind(view)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// [`ViewStore::add_view_to_dashboard`] inside a transaction.
    pub async fn add_view_to_dashboard_in(
        tx: &mut PgConnection,
        dashboard: i64,
        view: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO insight_dashboard_views (dashboard_id, view_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(dashboard)
        .bind(view)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    pub async fn remove_view_from_dashboard(&self, dashboard: i64, view: i64) -> Result<()> {
        sqlx::query("DELETE FROM insight_dashboard_views WHERE dashboard_id = $1 AND view_id = $2")
            .bind(dashboard)
            .bind(view)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_reach_users_orgs_and_everyone() {
        let ann = Principal {
            user: Some(UserId(1)),
            orgs: vec![7],
        };
        assert!(ann.reached_by(&[Grant::User(UserId(1))]));
        assert!(ann.reached_by(&[Grant::User(UserId(2)), Grant::Org(OrgId(7))]));
        assert!(ann.reached_by(&[Grant::Global]));
        assert!(!ann.reached_by(&[Grant::User(UserId(2)), Grant::Org(OrgId(8))]));
        assert!(ann.may_grant(&[Grant::User(UserId(1)), Grant::Org(OrgId(7)), Grant::Global]));
        assert!(!ann.may_grant(&[Grant::User(UserId(2))]));
        assert!(!ann.may_grant(&[Grant::Org(OrgId(8))]));
        let id = view_node_id("abc");
        assert_eq!(parse_view_node_id(&id).as_deref(), Some("abc"));
        assert_eq!(parse_view_node_id("VXNlcjox"), None);
        assert!(
            ViewFilters {
                include_repo_regex: Some(String::new()),
                ..ViewFilters::default()
            }
            .is_empty()
        );
    }
}
