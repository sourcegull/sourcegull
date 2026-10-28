//! Trends over GraphQL, on top of the `sourcegull-insights` engine:
//!
//! - views: `createLineChartSearchInsight`, `updateLineChartSearchInsight`,
//!   `createPieChartSearchInsight`, `updatePieChartSearchInsight`, `saveInsightAsNewView`,
//!   `deleteInsightView`, `insightViews` ([`view`]);
//! - dashboards: `insightsDashboards`, `createInsightsDashboard`, `updateInsightsDashboard`,
//!   `deleteInsightsDashboard`, `addInsightViewToDashboard`, `removeInsightViewFromDashboard`
//!   ([`dashboard`]);
//! - previews and scope checks: `searchInsightLivePreview`, `searchInsightPreview`,
//!   `validateScopedInsightQuery`, `previewRepositoriesFromQuery`,
//!   `previewLanguageStatistics` (for the language usage preview);
//! - the admin backfill queue: `insightAdminBackfillQueue`, `retryInsightSeriesBackfill`,
//!   `moveInsightSeriesBackfillToFrontOfQueue`, `moveInsightSeriesBackfillToBackOfQueue`
//!   ([`admin`]).
//!
//! Permissions: a view is visible to whoever it is granted to (its
//! creator) and to whoever sees a dashboard it is on; dashboards through their grants to
//! users, organizations or everyone; seeing is editing. Points are filtered by the viewer's
//! repository permissions and the view's filters at read time. The data export is served at
//! `/.api/insights/export/{id}` (`sourcegull_insights::http`).

pub mod admin;
pub mod dashboard;
mod feature;
pub mod view;

// `crate::context` and `crate::pagination`, as in the schema assembly.
use sourcegull_graphql_core::{context, pagination};

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use dashboard::{DashboardConnection, InsightsDashboardNode, list_dashboards, parse_dashboard_id};
use sourcegull_authn::AccessError;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::events::log_backend;
use sourcegull_graphql_core::ids::{parse_namespace_id, parse_user_id};
use sourcegull_graphql_core::node::{NodeFuture, NodeLoader};
use sourcegull_insights::access;
use sourcegull_insights::preview::preview;
use sourcegull_insights::views::{
    DisplayOptions, Grant, Presentation, Principal, SortDirection, SortMode, View, ViewFilters,
    ViewQuery, ViewSettings, ViewStore, parse_view_node_id,
};
use sourcegull_insights::{
    GenerationMethod, GroupBy, Interval, IntervalUnit, NewSeries, RepoScope, drilldown::DrillDown,
};
use sourcegull_search_web::SearchBackend;
use sourcegull_types::UserId;
use view::{InsightDataPoint, InsightViewNode, data_points};

use crate::context::{
    access_error, codes, db_error, error, repo_access, require_viewer, services, validation, viewer,
};

pub use dashboard::DASHBOARD_KIND;
pub use feature::InsightsGraphql;
pub use sourcegull_insights::views::VIEW_KIND;

fn views_store(ctx: &Context<'_>) -> Result<ViewStore> {
    Ok(ViewStore::new(services(ctx)?.db.pool().clone()))
}

/// The search backend (repo catalog, permissions, repo nodes) of the schema.
fn backend(ctx: &Context<'_>) -> Result<SearchBackend> {
    let s = services(ctx)?;
    Ok(match &s.app {
        Some(app) => app.search_backend(),
        None => sourcegull_services::search_backend(
            s.db.clone(),
            s.authz.clone(),
            s.repo_clients.clone(),
        ),
    })
}

/// The viewer as a grant principal (their user and organizations).
async fn principal(ctx: &Context<'_>) -> Result<Principal> {
    access::principal(&views_store(ctx)?, viewer(ctx))
        .await
        .map_err(access_error)
}

fn not_found_dashboard() -> async_graphql::Error {
    access_error(AccessError::NotFound("Dashboard"))
}

/// The view `id` (a view node ID) if `who` sees it.
async fn visible_view(ctx: &Context<'_>, who: &Principal, id: &str) -> Result<View> {
    let unique_id = parse_view_node_id(id)
        .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid trend ID {id:?}.")))?;
    access::visible_view(&views_store(ctx)?, who, &unique_id)
        .await
        .map_err(access_error)
}

/// The dashboard `id` if `who` sees it.
async fn visible_dashboard(ctx: &Context<'_>, who: &Principal, id: i64) -> Result<()> {
    access::require_dashboard_visible(&views_store(ctx)?, who, id)
        .await
        .map_err(access_error)
}

async fn load_dashboard(
    ctx: &Context<'_>,
    who: &Principal,
    id: i64,
) -> Result<InsightsDashboardNode> {
    views_store(ctx)?
        .list_dashboards(who, &[id], None, None, Some(1))
        .await
        .map_err(db_error)?
        .pop()
        .map(InsightsDashboardNode)
        .ok_or_else(not_found_dashboard)
}

/// The `Node` implementations of trends.
// Values are short-lived (one per `node(id:)` call), so the size difference does not matter.
#[allow(clippy::large_enum_variant)]
pub enum InsightsNode {
    /// A trend the viewer sees.
    View(InsightViewNode),
    /// A trends dashboard the viewer sees.
    Dashboard(InsightsDashboardNode),
}

/// The `node(id:)` loaders of trends and dashboards, for the schema's node type `N`.
pub fn node_loaders<N: From<InsightsNode> + Send + 'static>() -> [NodeLoader<N>; 1] {
    fn load<'a, N: From<InsightsNode> + Send + 'static>(
        ctx: &'a Context<'_>,
        kind: &'a str,
        id: &'a str,
    ) -> NodeFuture<'a, N> {
        Box::pin(async move { Ok(load_node(ctx, kind, id).await?.map(N::from)) })
    }
    [NodeLoader {
        kinds: &[VIEW_KIND, DASHBOARD_KIND],
        load: load::<N>,
    }]
}

/// Looks up a trend or dashboard node the viewer sees.
pub async fn load_node(ctx: &Context<'_>, kind: &str, id: &str) -> Result<Option<InsightsNode>> {
    match kind {
        VIEW_KIND => {
            let who = principal(ctx).await?;
            Ok(visible_view(ctx, &who, id)
                .await
                .ok()
                .map(|v| InsightsNode::View(InsightViewNode::new(v))))
        }
        DASHBOARD_KIND => {
            let who = principal(ctx).await?;
            let Ok(id) = parse_dashboard_id(id) else {
                return Ok(None);
            };
            Ok(load_dashboard(ctx, &who, id)
                .await
                .ok()
                .map(InsightsNode::Dashboard))
        }
        _ => Ok(None),
    }
}

// --- enums ---

/// What a grouped series groups by (`AUTHOR` and `DATE`, the commit's date, for commit and
/// diff searches only).
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum GroupByField {
    Repo,
    Lang,
    Path,
    Author,
    Date,
}

impl From<GroupBy> for GroupByField {
    fn from(g: GroupBy) -> Self {
        match g {
            GroupBy::Repo => Self::Repo,
            GroupBy::Lang => Self::Lang,
            GroupBy::Path => Self::Path,
            GroupBy::Author => Self::Author,
            GroupBy::Date => Self::Date,
        }
    }
}

impl GroupByField {
    fn to_engine(self) -> GroupBy {
        match self {
            Self::Repo => GroupBy::Repo,
            Self::Lang => GroupBy::Lang,
            Self::Path => GroupBy::Path,
            Self::Author => GroupBy::Author,
            Self::Date => GroupBy::Date,
        }
    }
}

/// The unit of a sample interval.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum TimeIntervalStepUnit {
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl From<TimeIntervalStepUnit> for IntervalUnit {
    fn from(u: TimeIntervalStepUnit) -> Self {
        match u {
            TimeIntervalStepUnit::Hour => Self::Hour,
            TimeIntervalStepUnit::Day => Self::Day,
            TimeIntervalStepUnit::Week => Self::Week,
            TimeIntervalStepUnit::Month => Self::Month,
            TimeIntervalStepUnit::Year => Self::Year,
        }
    }
}

impl From<IntervalUnit> for TimeIntervalStepUnit {
    fn from(u: IntervalUnit) -> Self {
        match u {
            IntervalUnit::Hour => Self::Hour,
            IntervalUnit::Day => Self::Day,
            IntervalUnit::Week => Self::Week,
            IntervalUnit::Month => Self::Month,
            IntervalUnit::Year => Self::Year,
        }
    }
}

/// How series are sorted.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum SeriesSortMode {
    ResultCount,
    Lexicographical,
    DateAdded,
}

impl From<SeriesSortMode> for SortMode {
    fn from(m: SeriesSortMode) -> Self {
        match m {
            SeriesSortMode::ResultCount => Self::ResultCount,
            SeriesSortMode::Lexicographical => Self::Lexicographical,
            SeriesSortMode::DateAdded => Self::DateAdded,
        }
    }
}

impl From<SortMode> for SeriesSortMode {
    fn from(m: SortMode) -> Self {
        match m {
            SortMode::ResultCount => Self::ResultCount,
            SortMode::Lexicographical => Self::Lexicographical,
            SortMode::DateAdded => Self::DateAdded,
        }
    }
}

#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "SCREAMING_SNAKE_CASE")]
pub enum SeriesSortDirection {
    Asc,
    Desc,
}

impl From<SeriesSortDirection> for SortDirection {
    fn from(d: SeriesSortDirection) -> Self {
        match d {
            SeriesSortDirection::Asc => Self::Asc,
            SeriesSortDirection::Desc => Self::Desc,
        }
    }
}

impl From<SortDirection> for SeriesSortDirection {
    fn from(d: SortDirection) -> Self {
        match d {
            SortDirection::Asc => Self::Asc,
            SortDirection::Desc => Self::Desc,
        }
    }
}

// --- inputs ---

/// A sample interval: every `value` `unit`s.
#[derive(InputObject, Debug, Clone)]
pub struct TimeIntervalStepInput {
    pub unit: TimeIntervalStepUnit,
    pub value: i32,
}

/// When a series records points.
#[derive(InputObject, Debug, Clone)]
pub struct TimeScopeInput {
    pub step_interval: Option<TimeIntervalStepInput>,
}

/// The repositories of a series: a list, or a query of repository filters (re-evaluated at
/// every recording); neither is every repository.
#[derive(InputObject, Debug, Clone)]
pub struct RepositoryScopeInput {
    pub repositories: Vec<String>,
    pub repository_criteria: Option<String>,
}

/// Options for a line chart.
#[derive(InputObject, Debug, Clone, Default)]
pub struct LineChartOptionsInput {
    pub title: Option<String>,
}

/// Options for a line chart's series.
#[derive(InputObject, Debug, Clone, Default)]
pub struct LineChartDataSeriesOptionsInput {
    pub label: Option<String>,
    pub line_color: Option<String>,
}

/// A data series of a line chart.
#[derive(InputObject, Debug, Clone)]
pub struct LineChartSearchInsightDataSeriesInput {
    /// The series to keep (updates); omit for a new series.
    pub series_id: Option<String>,
    pub query: String,
    pub options: LineChartDataSeriesOptionsInput,
    /// Overrides the trend's repository scope.
    pub repository_scope: Option<RepositoryScopeInput>,
    /// Overrides the trend's time scope.
    pub time_scope: Option<TimeScopeInput>,
    /// One series per string captured by the query's first capture group.
    pub generated_from_capture_groups: Option<bool>,
    /// One series per repository, language, path or author (needs a repository list).
    pub group_by: Option<GroupByField>,
}

/// Filters applied to a view's points.
#[derive(InputObject, Debug, Clone, Default)]
pub struct InsightViewFiltersInput {
    pub include_repo_regex: Option<String>,
    pub exclude_repo_regex: Option<String>,
    pub search_contexts: Option<Vec<String>>,
}

impl From<&InsightViewFiltersInput> for ViewFilters {
    fn from(f: &InsightViewFiltersInput) -> Self {
        Self {
            include_repo_regex: f.include_repo_regex.clone().filter(|r| !r.is_empty()),
            exclude_repo_regex: f.exclude_repo_regex.clone().filter(|r| !r.is_empty()),
            search_contexts: f
                .search_contexts
                .clone()
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.is_empty())
                .collect(),
        }
    }
}

#[derive(InputObject, Debug, Clone)]
pub struct SeriesSortOptionsInput {
    pub mode: SeriesSortMode,
    pub direction: SeriesSortDirection,
}

/// Which series a view shows.
#[derive(InputObject, Debug, Clone, Default)]
pub struct SeriesDisplayOptionsInput {
    pub sort_options: Option<SeriesSortOptionsInput>,
    /// Max number of series.
    pub limit: Option<i32>,
    /// Max number of points per series (at most 90).
    pub num_samples: Option<i32>,
}

impl From<&SeriesDisplayOptionsInput> for DisplayOptions {
    fn from(d: &SeriesDisplayOptionsInput) -> Self {
        Self {
            sort_mode: d.sort_options.as_ref().map(|s| s.mode.into()),
            sort_direction: d.sort_options.as_ref().map(|s| s.direction.into()),
            limit: d.limit.map(|l| l.max(0)),
            num_samples: d
                .num_samples
                .filter(|n| *n > 0)
                .map(|n| n.min(view::MAX_SAMPLES)),
        }
    }
}

/// A view's default filters and display options.
#[derive(InputObject, Debug, Clone)]
pub struct InsightViewControlsInput {
    pub filters: InsightViewFiltersInput,
    pub series_display_options: SeriesDisplayOptionsInput,
}

#[derive(InputObject, Debug, Clone)]
pub struct LineChartSearchInsightInput {
    pub data_series: Vec<LineChartSearchInsightDataSeriesInput>,
    /// Applies to every series that has none.
    pub repository_scope: Option<RepositoryScopeInput>,
    /// Applies to every series that has none.
    pub time_scope: Option<TimeScopeInput>,
    pub options: LineChartOptionsInput,
    /// Dashboards to add the trend to.
    pub dashboards: Option<Vec<ID>>,
    pub view_controls: Option<InsightViewControlsInput>,
}

#[derive(InputObject, Debug, Clone)]
pub struct UpdateLineChartSearchInsightInput {
    /// Every series of the chart; a series left out is removed.
    pub data_series: Vec<LineChartSearchInsightDataSeriesInput>,
    pub repository_scope: Option<RepositoryScopeInput>,
    pub time_scope: Option<TimeScopeInput>,
    pub presentation_options: LineChartOptionsInput,
    pub view_controls: InsightViewControlsInput,
}

/// Options for a pie chart.
#[derive(InputObject, Debug, Clone)]
pub struct PieChartOptionsInput {
    pub title: String,
    /// Languages with a smaller share of the lines are drawn as "Other".
    pub other_threshold: f64,
}

#[derive(InputObject, Debug, Clone)]
pub struct PieChartSearchInsightInput {
    pub query: String,
    pub repository_scope: RepositoryScopeInput,
    pub presentation_options: PieChartOptionsInput,
    pub dashboards: Option<Vec<ID>>,
}

#[derive(InputObject, Debug, Clone)]
pub struct UpdatePieChartSearchInsightInput {
    pub query: String,
    pub repository_scope: RepositoryScopeInput,
    pub presentation_options: PieChartOptionsInput,
}

#[derive(InputObject, Debug, Clone)]
pub struct SaveInsightAsNewViewInput {
    pub insight_view_id: ID,
    pub options: LineChartOptionsInput,
    pub dashboard: Option<ID>,
    pub view_controls: Option<InsightViewControlsInput>,
}

/// Grants of a dashboard. Users and organizations must be the viewer and the viewer's
/// organizations.
#[derive(InputObject, Debug, Clone, Default)]
pub struct InsightsPermissionGrantsInput {
    pub users: Option<Vec<ID>>,
    pub organizations: Option<Vec<ID>>,
    pub global: Option<bool>,
}

#[derive(InputObject, Debug, Clone)]
pub struct CreateInsightsDashboardInput {
    pub title: String,
    pub grants: InsightsPermissionGrantsInput,
}

#[derive(InputObject, Debug, Clone)]
pub struct UpdateInsightsDashboardInput {
    pub title: Option<String>,
    pub grants: Option<InsightsPermissionGrantsInput>,
}

#[derive(InputObject, Debug, Clone)]
pub struct AddInsightViewToDashboardInput {
    pub insight_view_id: ID,
    pub dashboard_id: ID,
}

#[derive(InputObject, Debug, Clone)]
pub struct RemoveInsightViewFromDashboardInput {
    pub insight_view_id: ID,
    pub dashboard_id: ID,
}

/// One series to preview.
#[derive(InputObject, Debug, Clone)]
pub struct SearchInsightLivePreviewInput {
    pub query: String,
    pub label: String,
    pub repository_scope: RepositoryScopeInput,
    pub time_scope: TimeScopeInput,
    pub generated_from_capture_groups: bool,
    pub group_by: Option<GroupByField>,
}

#[derive(InputObject, Debug, Clone)]
pub struct SearchSeriesPreviewInput {
    pub query: String,
    pub label: String,
    pub generated_from_capture_groups: bool,
    pub group_by: Option<GroupByField>,
}

/// Series to preview over one repository and time scope.
#[derive(InputObject, Debug, Clone)]
pub struct SearchInsightPreviewInput {
    pub repository_scope: RepositoryScopeInput,
    pub time_scope: TimeScopeInput,
    pub series: Vec<SearchSeriesPreviewInput>,
}

// --- payloads ---

pub struct InsightViewPayload(InsightViewNode);

#[Object]
impl InsightViewPayload {
    async fn view(&self) -> &InsightViewNode {
        &self.0
    }
}

pub struct InsightsDashboardPayload(InsightsDashboardNode);

#[Object]
impl InsightsDashboardPayload {
    async fn dashboard(&self) -> &InsightsDashboardNode {
        &self.0
    }
}

/// A previewed series.
pub struct SearchInsightLivePreviewSeries {
    label: String,
    points: Vec<view::DataPoint>,
}

#[Object]
impl SearchInsightLivePreviewSeries {
    async fn points(&self) -> Vec<InsightDataPoint> {
        self.points.iter().cloned().map(InsightDataPoint).collect()
    }

    async fn label(&self) -> &str {
        &self.label
    }
}

/// Whether a repository query is valid.
#[derive(SimpleObject)]
pub struct ScopedInsightQueryPayload {
    pub query: String,
    pub is_valid: bool,
    pub invalid_reason: Option<String>,
}

/// How many repositories a repository query matches for the viewer.
#[derive(SimpleObject)]
pub struct RepositoryPreviewPayload {
    pub query: String,
    pub number_of_repositories: Option<i32>,
}

// --- input conversion ---

/// A series input resolved against the trend-level scopes.
struct SeriesInput {
    series_id: Option<String>,
    label: String,
    stroke: String,
    new: NewSeries,
}

fn scope_of(scope: Option<&RepositoryScopeInput>) -> RepoScope {
    let Some(s) = scope else {
        return RepoScope::All;
    };
    let mut names: Vec<String> = s
        .repositories
        .iter()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .collect();
    names.sort();
    names.dedup();
    match (&s.repository_criteria, names.is_empty()) {
        (_, false) => RepoScope::List(names),
        (Some(c), true) if !c.trim().is_empty() => RepoScope::Query(c.trim().to_owned()),
        _ => RepoScope::All,
    }
}

fn method_of(
    capture_groups: Option<bool>,
    group_by: Option<GroupByField>,
) -> (GenerationMethod, Option<GroupBy>) {
    match group_by {
        Some(g) => (GenerationMethod::SearchCompute, Some(g.to_engine())),
        None if capture_groups == Some(true) => (GenerationMethod::MappingCompute, None),
        None => (GenerationMethod::Search, None),
    }
}

fn interval_of(t: Option<&TimeScopeInput>) -> Result<Interval, String> {
    let step = t
        .and_then(|t| t.step_interval.as_ref())
        .ok_or("A time scope is required.")?;
    if step.value < 1 {
        return Err("The step interval must be at least 1.".into());
    }
    Ok(Interval::new(
        step.unit.into(),
        u32::try_from(step.value).unwrap_or(1),
    ))
}

/// Checks and resolves the series of a line chart; problems are collected for the user.
fn series_inputs(
    series: &[LineChartSearchInsightDataSeriesInput],
    scope: Option<&RepositoryScopeInput>,
    time: Option<&TimeScopeInput>,
    created_by: UserId,
    problems: &mut Vec<String>,
) -> Vec<SeriesInput> {
    if series.is_empty() {
        problems.push("At least one data series is required.".into());
    }
    let mut out = Vec::new();
    for (i, s) in series.iter().enumerate() {
        let n = i + 1;
        let rscope = s.repository_scope.as_ref().or(scope);
        if let Some(r) = rscope
            && !r.repositories.is_empty()
            && r.repository_criteria
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty())
        {
            problems.push(format!(
                "Series {n}: give either a repository list or repository criteria, not both."
            ));
        }
        let scope = scope_of(rscope);
        if s.group_by.is_some() && !matches!(scope, RepoScope::List(_)) {
            problems.push(format!(
                "Series {n}: grouped series need a list of repositories."
            ));
        }
        let (method, group_by) = method_of(s.generated_from_capture_groups, s.group_by);
        let interval = match interval_of(s.time_scope.as_ref().or(time)) {
            Ok(i) => i,
            Err(e) => {
                problems.push(format!("Series {n}: {e}"));
                continue;
            }
        };
        let new = NewSeries {
            query: s.query.trim().to_owned(),
            method,
            group_by,
            scope,
            interval,
            created_by: Some(created_by),
        };
        if let Err(e) = sourcegull_insights::validate(&new) {
            problems.push(format!("Series {n}: {e}"));
            continue;
        }
        out.push(SeriesInput {
            series_id: s.series_id.clone().filter(|id| !id.is_empty()),
            label: s.options.label.clone().unwrap_or_default(),
            stroke: s.options.line_color.clone().unwrap_or_default(),
            new,
        });
    }
    out
}

/// Repositories listed by name must exist and be readable by the viewer.
async fn check_repositories(
    ctx: &Context<'_>,
    names: &[&String],
    problems: &mut Vec<String>,
) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let backend = backend(ctx)?;
    let catalog = backend
        .catalog
        .get(&backend.db)
        .await
        .map_err(|e| error(codes::INTERNAL, e))?;
    let access = repo_access(ctx).await?;
    for name in names {
        let readable = catalog
            .repos()
            .iter()
            .any(|r| &&r.name == name && access.can_read(sourcegull_types::RepoId(r.id)));
        if !readable {
            problems.push(format!("Repository not found: {name}."));
        }
    }
    Ok(())
}

fn grants_of(input: &InsightsPermissionGrantsInput) -> Result<Vec<Grant>> {
    let mut out = Vec::new();
    for u in input.users.iter().flatten() {
        out.push(Grant::User(parse_user_id(u)?));
    }
    for o in input.organizations.iter().flatten() {
        match parse_namespace_id(o)? {
            sourcegull_db::Namespace::Org(id) => out.push(Grant::Org(id)),
            sourcegull_db::Namespace::User(_) => {
                return Err(error(
                    codes::INVALID_INPUT,
                    format!("Invalid organization ID {:?}.", o.as_str()),
                ));
            }
        }
    }
    if input.global == Some(true) {
        out.push(Grant::Global);
    }
    out.sort_by_key(|g| match g {
        Grant::User(u) => (0, u.0),
        Grant::Org(o) => (1, o.0),
        Grant::Global => (2, 0),
    });
    out.dedup();
    Ok(out)
}

/// The dashboards a new view goes on; each must be visible to `who`.
async fn dashboard_ids(
    ctx: &Context<'_>,
    who: &Principal,
    ids: Option<&Vec<ID>>,
) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for id in ids.into_iter().flatten() {
        let id = parse_dashboard_id(id)?;
        visible_dashboard(ctx, who, id).await?;
        out.push(id);
    }
    Ok(out)
}

fn line_settings(
    title: Option<String>,
    controls: Option<&InsightViewControlsInput>,
) -> ViewSettings {
    ViewSettings {
        title: title.unwrap_or_default(),
        presentation: Presentation::Line,
        filters: controls.map(|c| (&c.filters).into()).unwrap_or_default(),
        display: controls
            .map(|c| (&c.series_display_options).into())
            .unwrap_or_default(),
        other_threshold: None,
        pie_query: None,
        pie_repositories: Vec::new(),
    }
}

fn pie_settings(
    query: &str,
    scope: &RepositoryScopeInput,
    p: &PieChartOptionsInput,
) -> ViewSettings {
    let mut repos: Vec<String> = scope
        .repositories
        .iter()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .collect();
    repos.sort();
    repos.dedup();
    ViewSettings {
        title: p.title.clone(),
        presentation: Presentation::Pie,
        filters: ViewFilters::default(),
        display: DisplayOptions::default(),
        other_threshold: Some(p.other_threshold),
        pie_query: Some(query.trim().to_owned()),
        pie_repositories: repos,
    }
}

/// Attaches the series of `inputs` to view `view` (new ones created, identical ones
/// shared), then detaches the view's other series. A series twice on a view is an error.
async fn set_view_series(
    tx: &mut sourcegull_db::sqlx::PgConnection,
    view: &View,
    inputs: &[SeriesInput],
) -> Result<()> {
    let mut kept: Vec<i64> = Vec::new();
    for (pos, input) in inputs.iter().enumerate() {
        // A series kept by ID and unchanged stays; otherwise the definition is matched or
        // created (series are never changed in place: other views may share them).
        let existing = input.series_id.as_ref().and_then(|sid| {
            view.series
                .iter()
                .find(|vs| &vs.series.series_id == sid)
                .filter(|vs| same_definition(&vs.series, &input.new))
        });
        let series_id = match existing {
            Some(vs) => vs.series.id,
            None => {
                ViewStore::series_for_in(tx, &input.new)
                    .await
                    .map_err(db_error)?
                    .id
            }
        };
        if kept.contains(&series_id) {
            return Err(validation(vec![format!(
                "Series {} is the same as another series of this trend.",
                pos + 1
            )]));
        }
        kept.push(series_id);
        ViewStore::attach_in(
            tx,
            view.id,
            series_id,
            &input.label,
            &input.stroke,
            i32::try_from(pos).unwrap_or(i32::MAX),
        )
        .await
        .map_err(db_error)?;
    }
    for vs in &view.series {
        if !kept.contains(&vs.series.id) {
            ViewStore::detach_in(tx, view.id, vs.series.id)
                .await
                .map_err(db_error)?;
        }
    }
    Ok(())
}

fn same_definition(s: &sourcegull_insights::Series, new: &NewSeries) -> bool {
    s.query == new.query
        && s.method == new.method
        && s.group_by == new.group_by
        && s.scope == new.scope
        && s.interval == new.interval
}

/// Reloads a view after a change.
async fn reload(ctx: &Context<'_>, unique_id: &str) -> Result<InsightViewPayload> {
    let view = views_store(ctx)?
        .view_by_unique_id(unique_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| access_error(AccessError::NotFound("Trend")))?;
    Ok(InsightViewPayload(InsightViewNode::new(view)))
}

// --- previews ---

async fn preview_series(
    ctx: &Context<'_>,
    new: &NewSeries,
    label: &str,
) -> Result<Vec<SearchInsightLivePreviewSeries>> {
    let backend = backend(ctx)?;
    let access = repo_access(ctx).await?;
    let points = usize::try_from(
        services(ctx)?
            .config
            .load()
            .value
            .insights_maximum_sample_size,
    )
    .unwrap_or(30)
    .clamp(1, 90);
    let out = preview(&backend, &access, new, points, chrono::Utc::now())
        .await
        .map_err(|e| error(codes::INVALID_INPUT, e))?;
    let repositories = match &new.scope {
        RepoScope::List(n) => n.as_slice(),
        _ => &[],
    };
    Ok(out
        .into_iter()
        .map(|p| {
            let capture = (new.method == GenerationMethod::MappingCompute)
                .then_some(p.value.as_deref())
                .flatten();
            let drill = DrillDown {
                query: &new.query,
                method: Some(new.method),
                capture,
                repositories,
                repository_criteria: match &new.scope {
                    RepoScope::Query(q) => Some(q.as_str()),
                    _ => None,
                },
                ..DrillDown::default()
            };
            SearchInsightLivePreviewSeries {
                label: match (&p.value, new.method) {
                    (_, GenerationMethod::Search) => label.to_owned(),
                    (Some(v), _) => v.clone(),
                    (None, _) => "Other".to_owned(),
                },
                points: data_points(&drill, &p.points),
            }
        })
        .collect())
}

fn preview_input(
    query: &str,
    capture_groups: bool,
    group_by: Option<GroupByField>,
    scope: &RepositoryScopeInput,
    time: &TimeScopeInput,
) -> Result<NewSeries> {
    let mut problems = Vec::new();
    let (method, group_by) = method_of(Some(capture_groups), group_by);
    let interval = interval_of(Some(time)).map_err(|e| validation(vec![e]))?;
    let new = NewSeries {
        query: query.trim().to_owned(),
        method,
        group_by,
        scope: scope_of(Some(scope)),
        interval,
        created_by: None,
    };
    if let Err(e) = sourcegull_insights::validate(&new) {
        problems.push(e);
    }
    if !problems.is_empty() {
        return Err(validation(problems));
    }
    Ok(new)
}

// --- roots ---

/// Trends queries.
#[derive(Default)]
pub struct InsightsQuery;

#[Object]
impl InsightsQuery {
    /// Dashboards visible to the viewer.
    async fn insights_dashboards(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        id: Option<ID>,
    ) -> Result<DashboardConnection> {
        let who = principal(ctx).await?;
        list_dashboards(ctx, &who, first, after, id, None).await
    }

    /// Fetch information related to the queue of backfilling trends (site admins only).
    /// `textSearch` filters by the trend title and the series label.
    #[allow(clippy::too_many_arguments)]
    async fn insight_admin_backfill_queue(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        before: Option<String>,
        #[graphql(default_with = "Some(admin::BackfillQueueOrderBy::State)")] order_by: Option<
            admin::BackfillQueueOrderBy,
        >,
        #[graphql(default_with = "Some(false)")] descending: Option<bool>,
        states: Option<Vec<admin::InsightQueueItemState>>,
        text_search: Option<String>,
    ) -> Result<admin::InsightBackfillQueueItemConnection> {
        admin::backfill_queue(
            ctx,
            first,
            last,
            after,
            before,
            order_by.unwrap_or_default(),
            descending.unwrap_or_default(),
            states,
            text_search,
        )
        .await
    }

    /// Trends visible to the viewer, ordered by ID. `filters` and
    /// `seriesDisplayOptions` override the views' own for this request.
    #[allow(clippy::too_many_arguments)]
    async fn insight_views(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        id: Option<ID>,
        exclude_ids: Option<Vec<ID>>,
        find: Option<String>,
        is_frozen: Option<bool>,
        filters: Option<InsightViewFiltersInput>,
        series_display_options: Option<SeriesDisplayOptionsInput>,
    ) -> Result<dashboard::InsightViewConnection> {
        let who = principal(ctx).await?;
        let parse = |id: &ID| {
            parse_view_node_id(id).ok_or_else(|| {
                error(
                    codes::INVALID_INPUT,
                    format!("Invalid trend ID {:?}.", id.as_str()),
                )
            })
        };
        let overrides_filters: Option<ViewFilters> = filters.as_ref().map(Into::into);
        if let Some(f) = &overrides_filters {
            sourcegull_insights::filters::validate(f).map_err(|e| validation(vec![e]))?;
        }
        let overrides_display: Option<DisplayOptions> =
            series_display_options.as_ref().map(Into::into);
        let limit = first.map(|f| i64::from(f.clamp(0, 1000)));
        let q = ViewQuery {
            unique_ids: id.iter().map(parse).collect::<Result<_>>()?,
            exclude_unique_ids: exclude_ids
                .iter()
                .flatten()
                .map(parse)
                .collect::<Result<_>>()?,
            find,
            after: after.filter(|a| !a.is_empty()),
            limit: limit.map(|l| l + 1),
        };
        // No view is frozen: there are no license limits.
        let (mut views, total) = if is_frozen == Some(true) {
            (Vec::new(), 0)
        } else {
            let store = views_store(ctx)?;
            (
                store.list_views(&who, &q).await.map_err(db_error)?,
                store.count_views(&who, &q).await.map_err(db_error)?,
            )
        };
        let more = limit.is_some_and(|l| i64::try_from(views.len()).unwrap_or(i64::MAX) > l);
        if let Some(l) = limit {
            views.truncate(usize::try_from(l).unwrap_or(usize::MAX));
        }
        let end_cursor = more
            .then(|| views.last().map(|v| v.unique_id.clone()))
            .flatten();
        Ok(dashboard::InsightViewConnection {
            total_count: Some(i32::try_from(total).unwrap_or(i32::MAX)),
            nodes: views
                .into_iter()
                .map(|v| {
                    InsightViewNode::new(v)
                        .with_overrides(overrides_filters.clone(), overrides_display)
                })
                .collect(),
            page_info: crate::pagination::PageInfo {
                has_next_page: more,
                end_cursor,
            },
        })
    }

    /// Counts one series over the viewer's repositories without storing it.
    async fn search_insight_live_preview(
        &self,
        ctx: &Context<'_>,
        input: SearchInsightLivePreviewInput,
    ) -> Result<Vec<SearchInsightLivePreviewSeries>> {
        require_viewer(ctx)?;
        let new = preview_input(
            &input.query,
            input.generated_from_capture_groups,
            input.group_by,
            &input.repository_scope,
            &input.time_scope,
        )?;
        preview_series(ctx, &new, &input.label).await
    }

    /// Counts series over the viewer's repositories without storing them.
    async fn search_insight_preview(
        &self,
        ctx: &Context<'_>,
        input: SearchInsightPreviewInput,
    ) -> Result<Vec<SearchInsightLivePreviewSeries>> {
        require_viewer(ctx)?;
        let mut out = Vec::new();
        for s in &input.series {
            let new = preview_input(
                &s.query,
                s.generated_from_capture_groups,
                s.group_by,
                &input.repository_scope,
                &input.time_scope,
            )?;
            out.extend(preview_series(ctx, &new, &s.label).await?);
        }
        Ok(out)
    }

    /// Whether `query` is a valid repository query for a trend's scope.
    async fn validate_scoped_insight_query(
        &self,
        ctx: &Context<'_>,
        query: String,
    ) -> Result<ScopedInsightQueryPayload> {
        require_viewer(ctx)?;
        let reason = sourcegull_insights::scope::criteria_filters(&query).err();
        Ok(ScopedInsightQueryPayload {
            query,
            is_valid: reason.is_none(),
            invalid_reason: reason,
        })
    }

    /// How many repositories the viewer may read that a repository query matches.
    async fn preview_repositories_from_query(
        &self,
        ctx: &Context<'_>,
        query: String,
    ) -> Result<RepositoryPreviewPayload> {
        require_viewer(ctx)?;
        let backend = backend(ctx)?;
        let catalog = backend
            .catalog
            .get(&backend.db)
            .await
            .map_err(|e| error(codes::INTERNAL, e))?;
        let repos = sourcegull_insights::scope::criteria_repos(&catalog, &query)
            .map_err(|e| validation(vec![e]))?;
        let access = repo_access(ctx).await?;
        let n = repos
            .iter()
            .filter(|r| access.can_read(sourcegull_types::RepoId(r.id)))
            .count();
        Ok(RepositoryPreviewPayload {
            query,
            number_of_repositories: Some(i32::try_from(n).unwrap_or(i32::MAX)),
        })
    }

    /// Lines and bytes per language of the listed repositories the viewer may read, as a
    /// language usage trend over them would show, for its live preview. Names
    /// that do not exist or are unreadable are left out alike.
    async fn preview_language_statistics(
        &self,
        ctx: &Context<'_>,
        repositories: Vec<String>,
    ) -> Result<Vec<view::LanguageStatistics>> {
        require_viewer(ctx)?;
        let mut names: Vec<String> = repositories
            .iter()
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty())
            .collect();
        names.sort();
        names.dedup();
        view::language_statistics(ctx, &names).await
    }
}

/// Trends mutations.
#[derive(Default)]
pub struct InsightsMutation;

#[Object]
impl InsightsMutation {
    /// Retry the backfill for a finished (completed or failed) trend series given the ID of
    /// its backfill queue item. Can only be used by a site admin.
    async fn retry_insight_series_backfill(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<admin::InsightBackfillQueueItem> {
        admin::change_backfill(ctx, &id, admin::QueueAction::Retry).await
    }

    /// Makes an unfinished trend series backfill the highest priority, given the ID of its
    /// backfill queue item. Can only be used by a site admin.
    async fn move_insight_series_backfill_to_front_of_queue(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<admin::InsightBackfillQueueItem> {
        admin::change_backfill(ctx, &id, admin::QueueAction::Front).await
    }

    /// Makes an unfinished trend series backfill the lowest priority, given the ID of its
    /// backfill queue item. Can only be used by a site admin.
    async fn move_insight_series_backfill_to_back_of_queue(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<admin::InsightBackfillQueueItem> {
        admin::change_backfill(ctx, &id, admin::QueueAction::Back).await
    }

    /// Creates a line chart of search series (shared with identical series of other
    /// trends); new series are backfilled in the background.
    async fn create_line_chart_search_insight(
        &self,
        ctx: &Context<'_>,
        input: LineChartSearchInsightInput,
    ) -> Result<InsightViewPayload> {
        let actor = require_viewer(ctx)?;
        let who = principal(ctx).await?;
        let mut problems = Vec::new();
        let series = series_inputs(
            &input.data_series,
            input.repository_scope.as_ref(),
            input.time_scope.as_ref(),
            actor.user_id,
            &mut problems,
        );
        let names: Vec<&String> = series
            .iter()
            .filter_map(|s| match &s.new.scope {
                RepoScope::List(n) => Some(n),
                _ => None,
            })
            .flatten()
            .collect();
        check_repositories(ctx, &names, &mut problems).await?;
        let settings = line_settings(input.options.title.clone(), input.view_controls.as_ref());
        if let Err(e) = sourcegull_insights::filters::validate(&settings.filters) {
            problems.push(e);
        }
        if !problems.is_empty() {
            return Err(validation(problems));
        }
        let dashboards = dashboard_ids(ctx, &who, input.dashboards.as_ref()).await?;
        let store = views_store(ctx)?;
        let mut tx = store.pool().begin().await.map_err(|e| db_error(e.into()))?;
        let view = ViewStore::create_view_in(
            &mut tx,
            &settings,
            &[Grant::User(actor.user_id)],
            Some(actor.user_id),
        )
        .await
        .map_err(db_error)?;
        set_view_series(&mut tx, &view, &series).await?;
        for d in dashboards {
            ViewStore::add_view_to_dashboard_in(&mut tx, d, view.id)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(|e| db_error(e.into()))?;
        log_backend(
            ctx,
            sourcegull_analytics::names::INSIGHT_ADDED,
            serde_json::json!({ "presentation": "line" }),
        );
        reload(ctx, &view.unique_id).await
    }

    /// Replaces a line chart's settings and series. Series kept by `seriesId` and unchanged
    /// keep their data; changed ones are replaced.
    async fn update_line_chart_search_insight(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateLineChartSearchInsightInput,
    ) -> Result<InsightViewPayload> {
        let actor = require_viewer(ctx)?;
        let who = principal(ctx).await?;
        let view = visible_view(ctx, &who, &id).await?;
        if view.presentation != Presentation::Line {
            return Err(validation(vec!["This trend is not a line chart.".into()]));
        }
        let mut problems = Vec::new();
        let series = series_inputs(
            &input.data_series,
            input.repository_scope.as_ref(),
            input.time_scope.as_ref(),
            actor.user_id,
            &mut problems,
        );
        let names: Vec<&String> = series
            .iter()
            .filter_map(|s| match &s.new.scope {
                RepoScope::List(n) => Some(n),
                _ => None,
            })
            .flatten()
            .collect();
        check_repositories(ctx, &names, &mut problems).await?;
        let settings = line_settings(
            input.presentation_options.title.clone(),
            Some(&input.view_controls),
        );
        if let Err(e) = sourcegull_insights::filters::validate(&settings.filters) {
            problems.push(e);
        }
        if !problems.is_empty() {
            return Err(validation(problems));
        }
        let store = views_store(ctx)?;
        let mut tx = store.pool().begin().await.map_err(|e| db_error(e.into()))?;
        ViewStore::update_view_in(&mut tx, view.id, &settings)
            .await
            .map_err(db_error)?;
        set_view_series(&mut tx, &view, &series).await?;
        tx.commit().await.map_err(|e| db_error(e.into()))?;
        reload(ctx, &view.unique_id).await
    }

    /// Creates a pie chart of the language statistics of the listed repositories.
    async fn create_pie_chart_search_insight(
        &self,
        ctx: &Context<'_>,
        input: PieChartSearchInsightInput,
    ) -> Result<InsightViewPayload> {
        let actor = require_viewer(ctx)?;
        let who = principal(ctx).await?;
        let settings = pie_settings(
            &input.query,
            &input.repository_scope,
            &input.presentation_options,
        );
        let mut problems = pie_problems(&settings);
        let names: Vec<&String> = settings.pie_repositories.iter().collect();
        check_repositories(ctx, &names, &mut problems).await?;
        if !problems.is_empty() {
            return Err(validation(problems));
        }
        let dashboards = dashboard_ids(ctx, &who, input.dashboards.as_ref()).await?;
        let store = views_store(ctx)?;
        let mut tx = store.pool().begin().await.map_err(|e| db_error(e.into()))?;
        let view = ViewStore::create_view_in(
            &mut tx,
            &settings,
            &[Grant::User(actor.user_id)],
            Some(actor.user_id),
        )
        .await
        .map_err(db_error)?;
        for d in dashboards {
            ViewStore::add_view_to_dashboard_in(&mut tx, d, view.id)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(|e| db_error(e.into()))?;
        log_backend(
            ctx,
            sourcegull_analytics::names::INSIGHT_ADDED,
            serde_json::json!({ "presentation": "pie" }),
        );
        reload(ctx, &view.unique_id).await
    }

    async fn update_pie_chart_search_insight(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdatePieChartSearchInsightInput,
    ) -> Result<InsightViewPayload> {
        let who = principal(ctx).await?;
        let view = visible_view(ctx, &who, &id).await?;
        if view.presentation != Presentation::Pie {
            return Err(validation(vec!["This trend is not a pie chart.".into()]));
        }
        let settings = pie_settings(
            &input.query,
            &input.repository_scope,
            &input.presentation_options,
        );
        let mut problems = pie_problems(&settings);
        let names: Vec<&String> = settings.pie_repositories.iter().collect();
        check_repositories(ctx, &names, &mut problems).await?;
        if !problems.is_empty() {
            return Err(validation(problems));
        }
        let store = views_store(ctx)?;
        let mut tx = store.pool().begin().await.map_err(|e| db_error(e.into()))?;
        ViewStore::update_view_in(&mut tx, view.id, &settings)
            .await
            .map_err(db_error)?;
        tx.commit().await.map_err(|e| db_error(e.into()))?;
        reload(ctx, &view.unique_id).await
    }

    /// Deletes a trend; series no other trend uses are deleted with it.
    async fn delete_insight_view(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let who = principal(ctx).await?;
        let view = visible_view(ctx, &who, &id).await?;
        views_store(ctx)?
            .delete_view(view.id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// A new line chart over the same series as an existing one, with its own title,
    /// filters and display options.
    async fn save_insight_as_new_view(
        &self,
        ctx: &Context<'_>,
        input: SaveInsightAsNewViewInput,
    ) -> Result<InsightViewPayload> {
        let actor = require_viewer(ctx)?;
        let who = principal(ctx).await?;
        let source = visible_view(ctx, &who, &input.insight_view_id).await?;
        if source.presentation != Presentation::Line {
            return Err(validation(vec![
                "Only line charts can be saved as a new view.".into(),
            ]));
        }
        let settings = line_settings(input.options.title.clone(), input.view_controls.as_ref());
        sourcegull_insights::filters::validate(&settings.filters)
            .map_err(|e| validation(vec![e]))?;
        let dashboards = dashboard_ids(
            ctx,
            &who,
            input.dashboard.as_ref().map(|d| vec![d.clone()]).as_ref(),
        )
        .await?;
        let store = views_store(ctx)?;
        let mut tx = store.pool().begin().await.map_err(|e| db_error(e.into()))?;
        let view = ViewStore::create_view_in(
            &mut tx,
            &settings,
            &[Grant::User(actor.user_id)],
            Some(actor.user_id),
        )
        .await
        .map_err(db_error)?;
        for (pos, vs) in source.series.iter().enumerate() {
            ViewStore::attach_in(
                &mut tx,
                view.id,
                vs.series.id,
                &vs.label,
                &vs.stroke,
                i32::try_from(pos).unwrap_or(i32::MAX),
            )
            .await
            .map_err(db_error)?;
        }
        for d in dashboards {
            ViewStore::add_view_to_dashboard_in(&mut tx, d, view.id)
                .await
                .map_err(db_error)?;
        }
        tx.commit().await.map_err(|e| db_error(e.into()))?;
        reload(ctx, &view.unique_id).await
    }

    /// Creates a dashboard with at least one grant; users and organizations granted must be
    /// the viewer and the viewer's organizations.
    async fn create_insights_dashboard(
        &self,
        ctx: &Context<'_>,
        input: CreateInsightsDashboardInput,
    ) -> Result<InsightsDashboardPayload> {
        let actor = require_viewer(ctx)?;
        let who = principal(ctx).await?;
        let grants = grants_of(&input.grants)?;
        if grants.is_empty() {
            return Err(validation(vec![
                "A dashboard needs at least one grant.".into(),
            ]));
        }
        access::require_may_grant(&who, &grants).map_err(access_error)?;
        let id = views_store(ctx)?
            .create_dashboard(input.title.trim(), &grants, Some(actor.user_id))
            .await
            .map_err(db_error)?;
        Ok(InsightsDashboardPayload(
            load_dashboard(ctx, &who, id).await?,
        ))
    }

    /// Renames a dashboard or replaces its grants (new grants as for creating one).
    async fn update_insights_dashboard(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateInsightsDashboardInput,
    ) -> Result<InsightsDashboardPayload> {
        let who = principal(ctx).await?;
        let id = parse_dashboard_id(&id)?;
        let current = load_dashboard(ctx, &who, id).await?;
        let grants = input.grants.as_ref().map(grants_of).transpose()?;
        if let Some(g) = &grants {
            if g.is_empty() {
                return Err(validation(vec![
                    "A dashboard needs at least one grant.".into(),
                ]));
            }
            let added: Vec<Grant> = g
                .iter()
                .filter(|x| !current.0.grants.contains(x))
                .copied()
                .collect();
            access::require_may_grant(&who, &added).map_err(access_error)?;
        }
        let title = input.title.as_deref().map(str::trim);
        let store = views_store(ctx)?;
        store
            .update_dashboard(id, title, grants.as_deref())
            .await
            .map_err(db_error)?;
        // Read back whoever sees it now: the viewer may have removed their own access.
        let dashboard = store
            .dashboard_by_id(id)
            .await
            .map_err(db_error)?
            .ok_or_else(not_found_dashboard)?;
        Ok(InsightsDashboardPayload(InsightsDashboardNode(dashboard)))
    }

    /// Deletes a dashboard; its views stay.
    async fn delete_insights_dashboard(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        let who = principal(ctx).await?;
        let id = parse_dashboard_id(&id)?;
        visible_dashboard(ctx, &who, id).await?;
        views_store(ctx)?
            .delete_dashboard(id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Adds a view the viewer sees to a dashboard the viewer sees.
    async fn add_insight_view_to_dashboard(
        &self,
        ctx: &Context<'_>,
        input: AddInsightViewToDashboardInput,
    ) -> Result<InsightsDashboardPayload> {
        let who = principal(ctx).await?;
        let dashboard = parse_dashboard_id(&input.dashboard_id)?;
        visible_dashboard(ctx, &who, dashboard).await?;
        let view = visible_view(ctx, &who, &input.insight_view_id).await?;
        views_store(ctx)?
            .add_view_to_dashboard(dashboard, view.id)
            .await
            .map_err(db_error)?;
        Ok(InsightsDashboardPayload(
            load_dashboard(ctx, &who, dashboard).await?,
        ))
    }

    /// Takes a view off a dashboard the viewer sees.
    async fn remove_insight_view_from_dashboard(
        &self,
        ctx: &Context<'_>,
        input: RemoveInsightViewFromDashboardInput,
    ) -> Result<InsightsDashboardPayload> {
        let who = principal(ctx).await?;
        let dashboard = parse_dashboard_id(&input.dashboard_id)?;
        visible_dashboard(ctx, &who, dashboard).await?;
        let unique_id = parse_view_node_id(&input.insight_view_id).ok_or_else(|| {
            error(
                codes::INVALID_INPUT,
                format!("Invalid trend ID {:?}.", input.insight_view_id.as_str()),
            )
        })?;
        if let Some(view) = views_store(ctx)?
            .view_by_unique_id(&unique_id)
            .await
            .map_err(db_error)?
        {
            views_store(ctx)?
                .remove_view_from_dashboard(dashboard, view.id)
                .await
                .map_err(db_error)?;
        }
        Ok(InsightsDashboardPayload(
            load_dashboard(ctx, &who, dashboard).await?,
        ))
    }
}

fn pie_problems(s: &ViewSettings) -> Vec<String> {
    let mut problems = Vec::new();
    if s.pie_repositories.is_empty() {
        problems.push("A language statistics trend needs a repository.".into());
    }
    if let Some(t) = s.other_threshold
        && !(0.0..=1.0).contains(&t)
    {
        problems.push("The other threshold must be between 0 and 1.".into());
    }
    problems
}
