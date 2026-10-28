//! The `InsightView` node: a chart's presentation, series definitions, filters, display
//! options, dashboards and its data series with points, status and drill-down queries.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use async_graphql::{Context, ID, Interface, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use sourcegull_graphql_core::shared::RepositoryRef;
use sourcegull_insights::drilldown::DrillDown;
use sourcegull_insights::views::{
    DisplayOptions, Presentation, SortDirection, SortMode, View, ViewFilters, ViewSeries,
    ViewStore, view_node_id,
};
use sourcegull_insights::{
    BackfillState, GenerationMethod, InsightStore, OTHER, RepoScope, Series,
};
use sourcegull_types::RepoAccess;
use tokio::sync::OnceCell;

use super::dashboard::{DashboardConnection, list_dashboards};
use super::{
    GroupByField, SeriesSortDirection, SeriesSortMode, TimeIntervalStepUnit, backend, principal,
    views_store,
};
use crate::context::{codes, db_error, error, repo_access, services, to_int};

/// Series shown per view by default.
pub const DEFAULT_SERIES_LIMIT: i32 = 20;
/// Points per series at most (the cap on `numSamples`).
pub const MAX_SAMPLES: i32 = 90;

/// One point of a series.
#[derive(Debug, Clone)]
pub struct DataPoint {
    pub time: DateTime<Utc>,
    pub value: i64,
    pub diff_query: Option<String>,
    pub point_in_time_query: Option<String>,
}

/// A trend data point.
pub struct InsightDataPoint(pub DataPoint);

#[Object]
impl InsightDataPoint {
    /// The time of this data point.
    async fn date_time(&self) -> DateTime<Utc> {
        self.0.time
    }

    /// The value of the trend at this point in time.
    async fn value(&self) -> f64 {
        self.0.value as f64
    }

    /// A search query that will show the diff between this point and the previous point.
    async fn diff_query(&self) -> Option<&str> {
        self.0.diff_query.as_deref()
    }

    /// A search query that will show the set of results represented by this point.
    async fn point_in_time_query(&self) -> Option<&str> {
        self.0.point_in_time_query.as_deref()
    }
}

/// Points with drill-down queries; `after` of the first is none.
pub fn data_points(drill: &DrillDown<'_>, points: &[(DateTime<Utc>, i64)]) -> Vec<DataPoint> {
    let mut prev = None;
    points
        .iter()
        .map(|(t, v)| {
            let p = DataPoint {
                time: *t,
                value: *v,
                diff_query: Some(drill.diff_query(prev, *t)),
                point_in_time_query: Some(drill.point_in_time_query(*t)),
            };
            prev = Some(*t);
            p
        })
        .collect()
}

/// A series of data about a trend: a series definition's count, or one captured or
/// grouped value of it.
#[derive(Clone)]
pub struct InsightsSeries {
    pub series_id: String,
    pub label: String,
    pub points: Vec<DataPoint>,
    pub series: Series,
}

#[Object]
impl InsightsSeries {
    /// Unique ID for the series (`<definition>-<value>` for a value of a capture group or
    /// grouped series).
    async fn series_id(&self) -> &str {
        &self.series_id
    }

    /// The label used to describe this series of data points.
    async fn label(&self) -> &str {
        &self.label
    }

    /// Data points over a time range (inclusive). The arguments are deprecated and ignored;
    /// the view's filters apply.
    #[allow(unused_variables)]
    async fn points(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        include_repo_regex: Option<String>,
        exclude_repo_regex: Option<String>,
    ) -> Vec<InsightDataPoint> {
        self.points.iter().cloned().map(InsightDataPoint).collect()
    }

    /// The status of this series of data, e.g. progress collecting it.
    async fn status(&self) -> InsightSeriesStatus {
        InsightSeriesStatus(self.series.clone())
    }
}

/// Status indicators for a series: its backfill's progress.
pub struct InsightSeriesStatus(pub Series);

#[Object]
impl InsightSeriesStatus {
    /// Rows stored for this series at the finest level (per repository, time and value).
    async fn total_points(&self, ctx: &Context<'_>) -> Result<i32> {
        let store = InsightStore::new(&services(ctx)?.db);
        Ok(to_int(store.point_rows(self.0.id).await.map_err(db_error)?))
    }

    /// Backfill jobs (one per repository) queued or running.
    async fn pending_jobs(&self, ctx: &Context<'_>) -> Result<i32> {
        Ok(to_int(progress(ctx, &self.0).await?.pending))
    }

    /// Backfill jobs completed.
    async fn completed_jobs(&self, ctx: &Context<'_>) -> Result<i32> {
        Ok(to_int(progress(ctx, &self.0).await?.completed))
    }

    /// Backfill jobs that failed for good: the series misses those repositories.
    async fn failed_jobs(&self, ctx: &Context<'_>) -> Result<i32> {
        Ok(to_int(progress(ctx, &self.0).await?.failed))
    }

    /// When the backfill was queued; null while it has not started.
    async fn backfill_queued_at(&self) -> Option<DateTime<Utc>> {
        self.0.backfill_queued_at
    }

    /// Whether the series is still loading data.
    async fn is_loading_data(&self, ctx: &Context<'_>) -> Result<Option<bool>> {
        let pending = progress(ctx, &self.0).await?.pending;
        Ok(Some(
            self.0.backfill_queued_at.is_none()
                || pending > 0
                || matches!(
                    self.0.backfill_state,
                    BackfillState::New | BackfillState::Running
                ),
        ))
    }

    /// Data points flagged terminally incomplete: the backfill failed in some repositories,
    /// or the query cannot be counted.
    async fn incomplete_datapoints(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<IncompleteDatapointAlert>> {
        let s = &self.0;
        let mut out = Vec::new();
        let time = s
            .backfill_times
            .last()
            .copied()
            .or(s.last_recorded_at)
            .unwrap_or(s.created_at);
        if let Some(e) = &s.error {
            out.push(IncompleteDatapointAlert::Generic(
                GenericIncompleteDatapointAlert {
                    time,
                    reason: e.clone(),
                    repos: Vec::new(),
                },
            ));
        }
        let store = InsightStore::new(&services(ctx)?.db);
        let failed = store.failed_repos(s.id).await.map_err(db_error)?;
        if !failed.is_empty() {
            out.push(IncompleteDatapointAlert::Generic(
                GenericIncompleteDatapointAlert {
                    time,
                    reason: "There was an issue during data processing that caused this \
                             point to be incomplete."
                        .into(),
                    repos: failed,
                },
            ));
        }
        Ok(out)
    }
}

/// The backfill progress of `s`: from its jobs while they exist, else from the counts kept
/// on the series when it completed.
async fn progress(ctx: &Context<'_>, s: &Series) -> Result<sourcegull_insights::Progress> {
    let store = InsightStore::new(&services(ctx)?.db);
    let p = store.progress(s.id).await.map_err(db_error)?;
    if p.total > 0 {
        return Ok(p);
    }
    Ok(sourcegull_insights::Progress {
        total: i64::from(s.repos_total),
        completed: i64::from(s.repos_done),
        failed: i64::from(s.repos_failed),
        pending: 0,
    })
}

/// A terminally incomplete data point.
#[derive(Interface)]
#[graphql(
    name = "IncompleteDatapointAlert",
    field(
        name = "time",
        ty = "DateTime<Utc>",
        desc = "The data point that is incomplete."
    )
)]
pub enum IncompleteDatapointAlert {
    Timeout(TimeoutDatapointAlert),
    Generic(GenericIncompleteDatapointAlert),
}

async fn repositories(ctx: &Context<'_>, ids: &[i32]) -> Result<Option<Vec<RepositoryRef>>> {
    if ids.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for id in ids {
        // Repositories the viewer cannot read are left out.
        if let Some(r) = sourcegull_graphql_core::repo::load_readable(ctx, i64::from(*id)).await? {
            out.push(RepositoryRef(r));
        }
    }
    Ok(Some(out))
}

/// A data point incomplete because counting timed out.
pub struct TimeoutDatapointAlert {
    pub time: DateTime<Utc>,
    pub repos: Vec<i32>,
}

#[Object]
impl TimeoutDatapointAlert {
    async fn time(&self) -> DateTime<Utc> {
        self.time
    }

    async fn repositories(&self, ctx: &Context<'_>) -> Result<Option<Vec<RepositoryRef>>> {
        repositories(ctx, &self.repos).await
    }
}

/// A data point incomplete for another reason.
pub struct GenericIncompleteDatapointAlert {
    pub time: DateTime<Utc>,
    pub reason: String,
    pub repos: Vec<i32>,
}

#[Object]
impl GenericIncompleteDatapointAlert {
    async fn time(&self) -> DateTime<Utc> {
        self.time
    }

    /// Why the point is incomplete.
    async fn reason(&self) -> &str {
        &self.reason
    }

    /// The repositories the point is incomplete for (those the viewer may read).
    async fn repositories(&self, ctx: &Context<'_>) -> Result<Option<Vec<RepositoryRef>>> {
        repositories(ctx, &self.repos).await
    }
}

/// The filters of a view.
#[derive(SimpleObject, Clone)]
pub struct InsightViewFilters {
    /// A regex on repository names to include.
    pub include_repo_regex: Option<String>,
    /// A regex on repository names to exclude.
    pub exclude_repo_regex: Option<String>,
    /// Search contexts whose repositories the points count.
    pub search_contexts: Option<Vec<String>>,
}

impl From<&ViewFilters> for InsightViewFilters {
    fn from(f: &ViewFilters) -> Self {
        Self {
            include_repo_regex: f.include_repo_regex.clone(),
            exclude_repo_regex: f.exclude_repo_regex.clone(),
            search_contexts: Some(f.search_contexts.clone()),
        }
    }
}

/// Series sort options.
#[derive(SimpleObject, Clone)]
pub struct SeriesSortOptions {
    pub mode: Option<SeriesSortMode>,
    pub direction: Option<SeriesSortDirection>,
}

/// Which series a view shows.
#[derive(SimpleObject, Clone)]
pub struct SeriesDisplayOptions {
    pub sort_options: SeriesSortOptions,
    /// Max number of series to return.
    pub limit: Option<i32>,
    /// Max number of samples to return for all the series on the view.
    pub num_samples: Option<i32>,
}

impl From<&DisplayOptions> for SeriesDisplayOptions {
    fn from(d: &DisplayOptions) -> Self {
        Self {
            sort_options: SeriesSortOptions {
                mode: d.sort_mode.map(Into::into),
                direction: d.sort_direction.map(Into::into),
            },
            limit: d.limit,
            num_samples: d.num_samples,
        }
    }
}

/// How the repositories of a series are chosen.
#[derive(Union)]
pub enum InsightRepositoryDefinition {
    InsightRepositoryScope(InsightRepositoryScope),
    RepositorySearchScope(RepositorySearchScope),
}

/// A list of repositories.
#[derive(SimpleObject)]
pub struct InsightRepositoryScope {
    pub repositories: Vec<String>,
}

/// Repositories chosen by a search, or all of them.
#[derive(SimpleObject)]
pub struct RepositorySearchScope {
    /// The repository query; empty for all repositories.
    pub search: String,
    pub all_repositories: bool,
}

fn repository_definition(scope: &RepoScope) -> InsightRepositoryDefinition {
    match scope {
        RepoScope::List(names) => {
            InsightRepositoryDefinition::InsightRepositoryScope(InsightRepositoryScope {
                repositories: names.clone(),
            })
        }
        RepoScope::Query(q) => {
            InsightRepositoryDefinition::RepositorySearchScope(RepositorySearchScope {
                search: q.clone(),
                all_repositories: false,
            })
        }
        RepoScope::All => {
            InsightRepositoryDefinition::RepositorySearchScope(RepositorySearchScope {
                search: String::new(),
                all_repositories: true,
            })
        }
    }
}

/// When a trend records points.
#[derive(Union)]
pub enum InsightTimeScope {
    InsightIntervalTimeScope(InsightIntervalTimeScope),
}

/// Every `value` `unit`s.
#[derive(SimpleObject)]
pub struct InsightIntervalTimeScope {
    pub unit: TimeIntervalStepUnit,
    pub value: i32,
}

fn time_scope(s: &Series) -> InsightTimeScope {
    InsightTimeScope::InsightIntervalTimeScope(InsightIntervalTimeScope {
        unit: s.interval.unit.into(),
        value: i32::try_from(s.interval.value).unwrap_or(i32::MAX),
    })
}

/// How a data series is generated.
#[derive(Union)]
pub enum InsightDataSeriesDefinition {
    SearchInsightDataSeriesDefinition(SearchInsightDataSeriesDefinition),
}

/// A series defined by a search query.
pub struct SearchInsightDataSeriesDefinition {
    series_id: String,
    query: String,
    scope: RepoScope,
    unit: TimeIntervalStepUnit,
    value: i32,
    method: Option<GenerationMethod>,
    group_by: Option<GroupByField>,
    calculated: bool,
}

#[Object]
impl SearchInsightDataSeriesDefinition {
    async fn series_id(&self) -> &str {
        &self.series_id
    }

    async fn query(&self) -> &str {
        &self.query
    }

    /// The repositories listed (empty for a repository query or all repositories).
    #[graphql(deprecation = "use repositoryDefinition instead")]
    async fn repository_scope(&self) -> InsightRepositoryScope {
        InsightRepositoryScope {
            repositories: match &self.scope {
                RepoScope::List(n) => n.clone(),
                _ => Vec::new(),
            },
        }
    }

    async fn repository_definition(&self) -> InsightRepositoryDefinition {
        repository_definition(&self.scope)
    }

    async fn time_scope(&self) -> InsightTimeScope {
        InsightTimeScope::InsightIntervalTimeScope(InsightIntervalTimeScope {
            unit: self.unit,
            value: self.value,
        })
    }

    /// Whether the series has a value per captured (or grouped) value.
    async fn generated_from_capture_groups(&self) -> bool {
        matches!(
            self.method,
            Some(GenerationMethod::MappingCompute | GenerationMethod::SearchCompute)
        )
    }

    /// Whether the series is recorded in the background (line charts) rather than computed
    /// when read (pie charts).
    async fn is_calculated(&self) -> bool {
        self.calculated
    }

    async fn group_by(&self) -> Option<GroupByField> {
        self.group_by
    }
}

fn definition(vs: &ViewSeries) -> SearchInsightDataSeriesDefinition {
    let s = &vs.series;
    SearchInsightDataSeriesDefinition {
        series_id: s.series_id.clone(),
        query: s.query.clone(),
        scope: s.scope.clone(),
        unit: s.interval.unit.into(),
        value: i32::try_from(s.interval.value).unwrap_or(i32::MAX),
        method: Some(s.method),
        group_by: s.group_by.map(Into::into),
        calculated: true,
    }
}

/// How a view is drawn.
#[derive(Union)]
pub enum InsightPresentation {
    LineChartInsightViewPresentation(LineChartInsightViewPresentation),
    PieChartInsightViewPresentation(PieChartInsightViewPresentation),
}

/// A line chart.
#[derive(SimpleObject)]
pub struct LineChartInsightViewPresentation {
    pub title: String,
    pub series_presentation: Vec<LineChartDataSeriesPresentation>,
}

/// One line of a line chart.
#[derive(SimpleObject)]
pub struct LineChartDataSeriesPresentation {
    pub series_id: String,
    pub label: String,
    pub color: String,
}

/// A pie chart of language statistics.
#[derive(SimpleObject)]
pub struct PieChartInsightViewPresentation {
    pub title: String,
    /// Languages with a smaller share of the lines are drawn as "Other".
    pub other_threshold: f64,
}

/// Lines and bytes of one language.
#[derive(SimpleObject)]
pub struct LanguageStatistics {
    pub name: String,
    pub total_lines: i32,
    pub total_bytes: f64,
}

/// A trend: a chart over data series (line) or of language statistics (pie).
pub struct InsightViewNode {
    pub view: View,
    /// Filters and display options passed to `insightViews`, overriding the view's.
    pub filters: Option<ViewFilters>,
    pub display: Option<DisplayOptions>,
    series: OnceCell<(Vec<InsightsSeries>, usize)>,
}

impl InsightViewNode {
    pub fn new(view: View) -> Self {
        Self {
            view,
            filters: None,
            display: None,
            series: OnceCell::new(),
        }
    }

    pub fn with_overrides(
        mut self,
        filters: Option<ViewFilters>,
        display: Option<DisplayOptions>,
    ) -> Self {
        self.filters = filters;
        self.display = display;
        self
    }

    fn filters_in_effect(&self) -> &ViewFilters {
        self.filters.as_ref().unwrap_or(&self.view.filters)
    }

    fn display_in_effect(&self) -> &DisplayOptions {
        self.display.as_ref().unwrap_or(&self.view.display)
    }

    /// The data series (sorted and limited) and how many there were before the limit.
    async fn compute_series(&self, ctx: &Context<'_>) -> Result<&(Vec<InsightsSeries>, usize)> {
        self.series
            .get_or_try_init(|| async {
                let access = repo_access(ctx).await?;
                let filters = self.filters_in_effect();
                let only = if filters.is_empty() {
                    None
                } else {
                    let backend = backend(ctx)?;
                    let catalog = backend
                        .catalog
                        .get(&backend.db)
                        .await
                        .map_err(|e| error(codes::INTERNAL, e))?;
                    sourcegull_insights::filters::resolve(
                        &backend.db,
                        &catalog,
                        filters,
                        crate::context::viewer(ctx).map(|a| a.user_id),
                    )
                    .await
                    .map_err(|e| error(codes::INVALID_INPUT, e))?
                };
                let display = self.display_in_effect();
                let samples = display
                    .num_samples
                    .filter(|n| *n > 0)
                    .map_or(MAX_SAMPLES, |n| n.min(MAX_SAMPLES));
                let store = InsightStore::new(&services(ctx)?.db);
                let mut all = Vec::new();
                for vs in &self.view.series {
                    all.extend(
                        series_of(&store, vs, &access, only.as_ref(), filters, samples).await?,
                    );
                }
                let total = all.len();
                Ok::<_, async_graphql::Error>((sort_and_limit(all, display), total))
            })
            .await
    }
}

/// The data series of one view series: its count, or one per captured or grouped value.
async fn series_of(
    store: &InsightStore,
    vs: &ViewSeries,
    access: &RepoAccess,
    only: Option<&sourcegull_insights::RoaringBitmap>,
    filters: &ViewFilters,
    samples: i32,
) -> Result<Vec<InsightsSeries>> {
    let s = &vs.series;
    let read = sourcegull_insights::points_in(store, s, access, only)
        .await
        .map_err(db_error)?;
    let mut by_value: BTreeMap<String, Vec<(DateTime<Utc>, i64)>> = BTreeMap::new();
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    for p in &read.points {
        if times.last() != Some(&p.time) {
            times.push(p.time);
        }
        by_value
            .entry(p.value.clone())
            .or_default()
            .push((p.time, p.count));
    }
    let keep = usize::try_from(samples).unwrap_or(usize::MAX);
    let cut = times.len().saturating_sub(keep);
    let times = &times[cut..];
    // Every value has a point at every time (0 where it had none).
    let filled = |pts: &[(DateTime<Utc>, i64)]| -> Vec<(DateTime<Utc>, i64)> {
        times
            .iter()
            .map(|t| {
                (
                    *t,
                    pts.iter().filter(|(pt, _)| pt == t).map(|(_, c)| *c).sum(),
                )
            })
            .collect()
    };
    let drill = |capture| drill_down(s, filters, capture);
    if s.method == GenerationMethod::Search {
        let all: Vec<(DateTime<Utc>, i64)> =
            filled(&by_value.values().flatten().copied().collect::<Vec<_>>());
        return Ok(vec![InsightsSeries {
            series_id: s.series_id.clone(),
            label: vs.label.clone(),
            points: data_points(&drill(None), &all),
            series: s.clone(),
        }]);
    }
    let mut out = Vec::new();
    for (value, pts) in &by_value {
        let pts = filled(pts);
        if value == OTHER {
            if pts.iter().all(|(_, c)| *c == 0) {
                continue;
            }
            out.push(InsightsSeries {
                series_id: format!("{}-other", s.series_id),
                label: "Other".into(),
                points: data_points(&drill(None), &pts),
                series: s.clone(),
            });
            continue;
        }
        let capture = (s.method == GenerationMethod::MappingCompute).then_some(value.as_str());
        let repo_value = [value.clone()];
        let mut d = drill(capture);
        if s.group_by == Some(sourcegull_insights::GroupBy::Repo) {
            d.repositories = &repo_value;
        }
        out.push(InsightsSeries {
            series_id: format!("{}-{value}", s.series_id),
            label: value.clone(),
            points: data_points(&d, &pts),
            series: s.clone(),
        });
    }
    if out.is_empty() {
        // No value yet (e.g. still backfilling): one empty series carries the status.
        out.push(InsightsSeries {
            series_id: s.series_id.clone(),
            label: vs.label.clone(),
            points: Vec::new(),
            series: s.clone(),
        });
    }
    Ok(out)
}

/// The drill-down of a series' points under `filters`.
fn drill_down<'a>(
    s: &'a Series,
    filters: &'a ViewFilters,
    capture: Option<&'a str>,
) -> DrillDown<'a> {
    DrillDown {
        query: &s.query,
        method: Some(s.method),
        capture,
        repositories: match &s.scope {
            RepoScope::List(n) => n.as_slice(),
            _ => &[],
        },
        repository_criteria: match &s.scope {
            RepoScope::Query(q) => Some(q.as_str()),
            _ => None,
        },
        include_repo_regex: filters.include_repo_regex.as_deref(),
        exclude_repo_regex: filters.exclude_repo_regex.as_deref(),
        context: filters.search_contexts.first().map(String::as_str),
    }
}

/// A lenient semantic version (`v1.2.3`, `1.2`, `2`), for sorting labels.
fn semver(s: &str) -> Option<(u64, u64, u64, String)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let (core, pre) = match s.split_once(['-', '+']) {
        Some((c, p)) => (c, p.to_owned()),
        None => (s, String::new()),
    };
    let mut parts = core.split('.');
    let mut next = |required: bool| -> Option<u64> {
        match parts.next() {
            Some(p) => p.parse().ok(),
            None if required => None,
            None => Some(0),
        }
    };
    let v = (next(true)?, next(false)?, next(false)?);
    if parts.next().is_some() {
        return None;
    }
    Some((v.0, v.1, v.2, pre))
}

/// Ascending label order: versions before other labels, versions by version, the rest by
/// string.
fn label_order(a: &str, b: &str) -> Ordering {
    match (semver(a), semver(b)) {
        (Some(x), Some(y)) => {
            let ord = (x.0, x.1, x.2).cmp(&(y.0, y.1, y.2));
            // A pre-release sorts before its release.
            ord.then_with(|| match (x.3.is_empty(), y.3.is_empty()) {
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => x.3.cmp(&y.3),
            })
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

/// Sorts series as the view's display options say (default: newest value, descending) and
/// keeps the first `limit` (default 20).
pub fn sort_and_limit(mut series: Vec<InsightsSeries>, d: &DisplayOptions) -> Vec<InsightsSeries> {
    let mode = d.sort_mode.unwrap_or(SortMode::ResultCount);
    let direction = d.sort_direction.unwrap_or(SortDirection::Desc);
    let limit = d.limit.unwrap_or(DEFAULT_SERIES_LIMIT);
    series.sort_by(|a, b| label_order(&a.label, &b.label));
    let newest = |s: &InsightsSeries| s.points.last().map_or(0, |p| p.value);
    let oldest = |s: &InsightsSeries| s.points.first().map(|p| p.time);
    match (mode, direction) {
        (SortMode::ResultCount, SortDirection::Asc) => series.sort_by_key(newest),
        (SortMode::ResultCount, SortDirection::Desc) => {
            series.sort_by_key(|s| std::cmp::Reverse(newest(s)))
        }
        (SortMode::Lexicographical, SortDirection::Asc) => {}
        (SortMode::Lexicographical, SortDirection::Desc) => {
            series.sort_by(|a, b| label_order(&b.label, &a.label))
        }
        (SortMode::DateAdded, SortDirection::Asc) => {
            series.sort_by_key(|s| oldest(s).unwrap_or(DateTime::<Utc>::MAX_UTC))
        }
        (SortMode::DateAdded, SortDirection::Desc) => {
            series.sort_by_key(|s| std::cmp::Reverse(oldest(s).unwrap_or(DateTime::<Utc>::MIN_UTC)))
        }
    }
    series.truncate(usize::try_from(limit.max(0)).unwrap_or(0));
    series
}

#[Object(name = "InsightView")]
impl InsightViewNode {
    pub async fn id(&self) -> ID {
        ID(view_node_id(&self.view.unique_id))
    }

    /// The filters saved on the view.
    async fn default_filters(&self) -> InsightViewFilters {
        (&self.view.filters).into()
    }

    /// The filters applied to the data (the saved ones unless overridden).
    async fn applied_filters(&self) -> InsightViewFilters {
        self.filters_in_effect().into()
    }

    /// The time series, as the viewer may see them, sorted and limited by the applied
    /// display options. Empty for pie charts.
    async fn data_series(&self, ctx: &Context<'_>) -> Result<Vec<InsightsSeries>> {
        if self.view.presentation == Presentation::Pie {
            return Ok(Vec::new());
        }
        Ok(self.compute_series(ctx).await?.0.clone())
    }

    async fn presentation(&self) -> InsightPresentation {
        match self.view.presentation {
            Presentation::Pie => InsightPresentation::PieChartInsightViewPresentation(
                PieChartInsightViewPresentation {
                    title: self.view.title.clone(),
                    other_threshold: self.view.other_threshold.unwrap_or(0.03),
                },
            ),
            Presentation::Line => InsightPresentation::LineChartInsightViewPresentation(
                LineChartInsightViewPresentation {
                    title: self.view.title.clone(),
                    series_presentation: self
                        .view
                        .series
                        .iter()
                        .map(|vs| LineChartDataSeriesPresentation {
                            series_id: vs.series.series_id.clone(),
                            label: vs.label.clone(),
                            color: vs.stroke.clone(),
                        })
                        .collect(),
                },
            ),
        }
    }

    /// How each data series is generated.
    async fn data_series_definitions(&self) -> Vec<InsightDataSeriesDefinition> {
        if self.view.presentation == Presentation::Pie {
            return vec![
                InsightDataSeriesDefinition::SearchInsightDataSeriesDefinition(
                    SearchInsightDataSeriesDefinition {
                        series_id: self.view.unique_id.clone(),
                        query: self.view.pie_query.clone().unwrap_or_default(),
                        scope: RepoScope::List(self.view.pie_repositories.clone()),
                        unit: TimeIntervalStepUnit::Month,
                        value: 1,
                        method: None,
                        group_by: None,
                        calculated: false,
                    },
                ),
            ];
        }
        self.view
            .series
            .iter()
            .map(|vs| {
                InsightDataSeriesDefinition::SearchInsightDataSeriesDefinition(definition(vs))
            })
            .collect()
    }

    /// On how many dashboards the view is, whoever may see them.
    async fn dashboard_reference_count(&self, ctx: &Context<'_>) -> Result<i32> {
        Ok(to_int(
            views_store(ctx)?
                .dashboard_reference_count(self.view.id)
                .await
                .map_err(db_error)?,
        ))
    }

    /// Always false: there are no license limits.
    async fn is_frozen(&self) -> bool {
        false
    }

    async fn default_series_display_options(&self) -> SeriesDisplayOptions {
        (&self.view.display).into()
    }

    async fn applied_series_display_options(&self) -> SeriesDisplayOptions {
        self.display_in_effect().into()
    }

    /// The dashboards holding this view that the viewer sees.
    async fn dashboards(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        id: Option<ID>,
    ) -> Result<Option<DashboardConnection>> {
        let who = principal(ctx).await?;
        Ok(Some(
            list_dashboards(ctx, &who, first, after, id, Some(self.view.id)).await?,
        ))
    }

    /// The number of data series before the display limit.
    async fn series_count(&self, ctx: &Context<'_>) -> Result<Option<i32>> {
        if self.view.presentation == Presentation::Pie {
            return Ok(Some(0));
        }
        Ok(Some(to_int(
            i64::try_from(self.compute_series(ctx).await?.1).unwrap_or(i64::MAX),
        )))
    }

    /// The repositories of the view (its first series' scope).
    async fn repository_definition(&self) -> Result<InsightRepositoryDefinition> {
        if self.view.presentation == Presentation::Pie {
            return Ok(repository_definition(&RepoScope::List(
                self.view.pie_repositories.clone(),
            )));
        }
        let first = self
            .view
            .series
            .first()
            .ok_or_else(|| error(codes::INTERNAL, "The trend has no series."))?;
        Ok(repository_definition(&first.series.scope))
    }

    /// The view's sample interval (its first series').
    async fn time_scope(&self) -> Result<InsightTimeScope> {
        if self.view.presentation == Presentation::Pie {
            return Ok(InsightTimeScope::InsightIntervalTimeScope(
                InsightIntervalTimeScope {
                    unit: TimeIntervalStepUnit::Month,
                    value: 1,
                },
            ));
        }
        let first = self
            .view
            .series
            .first()
            .ok_or_else(|| error(codes::INTERNAL, "The trend has no series."))?;
        Ok(time_scope(&first.series))
    }

    /// Pie charts: lines and bytes per language of the view's repositories (those the
    /// viewer may read) at their default branch, most lines first. Indexed heads are read
    /// from the search index's metadata; others are counted with per-file counts cached.
    /// Empty for line charts.
    async fn language_statistics(&self, ctx: &Context<'_>) -> Result<Vec<LanguageStatistics>> {
        if self.view.presentation != Presentation::Pie {
            return Ok(Vec::new());
        }
        language_statistics(ctx, &self.view.pie_repositories).await
    }
}

/// Lines and bytes per language of the repositories named in `names` that the viewer may
/// read, most lines first; repositories that do not exist or are unreadable are left out
/// alike.
pub async fn language_statistics(
    ctx: &Context<'_>,
    names: &[String],
) -> Result<Vec<LanguageStatistics>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let backend = backend(ctx)?;
    let catalog = backend
        .catalog
        .get(&backend.db)
        .await
        .map_err(|e| error(codes::INTERNAL, e))?;
    let access = repo_access(ctx).await?;
    let repos: Vec<(i32, String)> = catalog
        .repos()
        .iter()
        .filter(|r| names.contains(&r.name))
        .filter(|r| access.can_read(sourcegull_types::RepoId(r.id)))
        .map(|r| (r.id, r.name.clone()))
        .collect();
    if repos.is_empty() {
        return Ok(Vec::new());
    }
    let stats = sourcegull_insights::langstats::language_stats(&backend.repo_clients, &repos)
        .await
        .map_err(|e| error(codes::UNAVAILABLE, e))?;
    Ok(stats
        .languages
        .into_iter()
        .map(|l| LanguageStatistics {
            name: l.name,
            total_lines: i32::try_from(l.total_lines).unwrap_or(i32::MAX),
            total_bytes: l.total_bytes as f64,
        })
        .collect())
}

/// Loads views `who` sees by internal id, as nodes.
pub async fn view_nodes(ctx: &Context<'_>, ids: &[i64]) -> Result<Vec<InsightViewNode>> {
    let views: ViewStore = views_store(ctx)?;
    Ok(views
        .views_by_ids(ids)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(InsightViewNode::new)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(label: &str, values: &[i64]) -> InsightsSeries {
        let t0 = DateTime::<Utc>::UNIX_EPOCH;
        InsightsSeries {
            series_id: label.into(),
            label: label.into(),
            points: values
                .iter()
                .enumerate()
                .map(|(i, v)| DataPoint {
                    time: t0 + chrono::TimeDelta::days(i as i64),
                    value: *v,
                    diff_query: None,
                    point_in_time_query: None,
                })
                .collect(),
            series: Series {
                id: 0,
                series_id: String::new(),
                query: String::new(),
                method: GenerationMethod::Search,
                group_by: None,
                scope: RepoScope::All,
                interval: sourcegull_insights::Interval::new(
                    sourcegull_insights::IntervalUnit::Day,
                    1,
                ),
                created_by: None,
                created_at: t0,
                deleted_at: None,
                backfill_state: BackfillState::Completed,
                backfill_queued_at: None,
                backfill_completed_at: None,
                backfill_times: Vec::new(),
                repos_total: 0,
                repos_done: 0,
                repos_failed: 0,
                repos: Default::default(),
                last_recorded_at: None,
                error: None,
                backfill_priority: None,
            },
        }
    }

    fn labels(v: &[InsightsSeries]) -> Vec<&str> {
        v.iter().map(|s| s.label.as_str()).collect()
    }

    #[test]
    fn series_sort_by_count_label_or_date_and_are_limited() {
        let all = vec![
            s("b", &[1, 5]),
            s("a", &[3, 5]),
            s("v1.10.0", &[9]),
            s("v1.9.0", &[2, 2, 1]),
        ];
        let d = DisplayOptions::default();
        assert_eq!(
            labels(&sort_and_limit(all.clone(), &d)),
            ["v1.10.0", "a", "b", "v1.9.0"]
        );
        let lex = |dir| DisplayOptions {
            sort_mode: Some(SortMode::Lexicographical),
            sort_direction: Some(dir),
            limit: Some(3),
            ..DisplayOptions::default()
        };
        assert_eq!(
            labels(&sort_and_limit(all.clone(), &lex(SortDirection::Asc))),
            ["v1.9.0", "v1.10.0", "a"]
        );
        assert_eq!(
            labels(&sort_and_limit(all.clone(), &lex(SortDirection::Desc))),
            ["b", "a", "v1.10.0"]
        );
        let asc = DisplayOptions {
            sort_mode: Some(SortMode::ResultCount),
            sort_direction: Some(SortDirection::Asc),
            limit: Some(1),
            ..DisplayOptions::default()
        };
        assert_eq!(labels(&sort_and_limit(all, &asc)), ["v1.9.0"]);
        assert_eq!(label_order("1.0.0-rc1", "1.0.0"), Ordering::Less);
    }
}
