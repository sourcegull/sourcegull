//! Usage analytics for site admins: `site.analytics { search, users, codeIntel,
//! notebooks, batchChanges, codeInsights, … }` in the `AnalyticsStatItem` shape
//! (`summary` and `nodes` by day or week), `site.usageStatistics` (DAU/WAU/MAU) and
//! `site.users` (users with their last activity and event counts).
//!
//! Everything is read from the rollups of the local `event_logs` (`sourcegull-analytics`), which
//! the worker refreshes every half hour; changesets come from the bulk changes tables.
//! Further fields: `search.searchesByType`, `search.latency`, `codeMonitors`,
//! `notebooks`-like `views`/`creations` on trends and bulk changes, `users.signIns`,
//! and `updatedAt`.

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject};
use chrono::{DateTime, NaiveDate, Utc};
use sourcegull_analytics::names::{self, CODE_HOST_INTEGRATION};
use sourcegull_analytics::query::{self, DateRange, EventFilter, Grouping, UserOrder, Window};
use sourcegull_analytics::rollup::Period;
use sourcegull_services::relay::marshal_id;

// `crate::context`, as in the schema assembly.
use sourcegull_graphql_core::context;

use crate::context::{codes, db_error, error, repo_access, services};
use sourcegull_graphql_core::ids::USER_KIND;

/// A predefined range of days, ending today.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnalyticsDateRange {
    LastThreeMonths,
    LastMonth,
    LastWeek,
    /// Not supported: fails with `INVALID_INPUT`.
    Custom,
}

/// How a time series is bucketed.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnalyticsGrouping {
    Daily,
    Weekly,
}

fn window(
    today: NaiveDate,
    range: Option<AnalyticsDateRange>,
    grouping: Option<AnalyticsGrouping>,
) -> Result<Window> {
    let range = match range.unwrap_or(AnalyticsDateRange::LastMonth) {
        AnalyticsDateRange::LastThreeMonths => DateRange::LastThreeMonths,
        AnalyticsDateRange::LastMonth => DateRange::LastMonth,
        AnalyticsDateRange::LastWeek => DateRange::LastWeek,
        AnalyticsDateRange::Custom => {
            return Err(error(
                codes::INVALID_INPUT,
                "Custom date ranges are not supported.",
            ));
        }
    };
    let grouping = match grouping.unwrap_or(AnalyticsGrouping::Weekly) {
        AnalyticsGrouping::Daily => Grouping::Daily,
        AnalyticsGrouping::Weekly => Grouping::Weekly,
    };
    Ok(Window::new(range, grouping, today))
}

fn iso(day: NaiveDate) -> String {
    sourcegull_analytics::rollup::day_start(day)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Events (and users) of one bucket.
#[derive(SimpleObject)]
pub struct AnalyticsStatItemNode {
    /// The first day of the bucket, in ISO format.
    date: String,
    /// Events.
    count: f64,
    /// Distinct users who triggered the events, including anonymous visitors and deleted
    /// users.
    unique_users: f64,
    /// Distinct users who still exist.
    registered_users: f64,
}

/// Totals over the date range.
#[derive(SimpleObject)]
pub struct AnalyticsStatItemSummary {
    total_count: f64,
    total_unique_users: f64,
    total_registered_users: f64,
}

/// A time series with its totals.
#[derive(SimpleObject)]
pub struct AnalyticsStatItem {
    summary: AnalyticsStatItemSummary,
    nodes: Vec<AnalyticsStatItemNode>,
}

impl From<query::StatItem> for AnalyticsStatItem {
    fn from(s: query::StatItem) -> Self {
        Self {
            summary: AnalyticsStatItemSummary {
                total_count: s.summary.total_count as f64,
                total_unique_users: s.summary.total_unique_users as f64,
                total_registered_users: s.summary.total_registered_users as f64,
            },
            nodes: s
                .nodes
                .into_iter()
                .map(|n| AnalyticsStatItemNode {
                    date: iso(n.date),
                    count: n.count as f64,
                    unique_users: n.unique_users as f64,
                    registered_users: n.registered_users as f64,
                })
                .collect(),
        }
    }
}

async fn stat(ctx: &Context<'_>, filter: EventFilter, w: &Window) -> Result<AnalyticsStatItem> {
    Ok(query::events(&services(ctx)?.db, &filter, w)
        .await
        .map_err(db_error)?
        .into())
}

async fn named(ctx: &Context<'_>, names: &[&str], w: &Window) -> Result<AnalyticsStatItem> {
    stat(ctx, EventFilter::names(names), w).await
}

/// Usage analytics (site admins only).
pub struct Analytics {
    today: NaiveDate,
}

impl Analytics {
    /// The analytics as of now.
    pub fn new() -> Self {
        Self {
            today: Utc::now().date_naive(),
        }
    }
}

impl Default for Analytics {
    fn default() -> Self {
        Self::new()
    }
}

#[Object]
impl Analytics {
    /// Searches, result clicks and file views.
    async fn search(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsSearchResult> {
        Ok(AnalyticsSearchResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Notebook creations, views and block runs.
    async fn notebooks(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsNotebooksResults> {
        Ok(AnalyticsNotebooksResults(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Active users and how often they use the instance.
    async fn users(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsUsersResult> {
        Ok(AnalyticsUsersResult {
            window: window(self.today, date_range, grouping)?,
            today: self.today,
        })
    }

    /// Code navigation.
    async fn code_intel(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsCodeIntelResult> {
        Ok(AnalyticsCodeIntelResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Code navigation events by language and precision.
    async fn code_intel_by_language(
        &self,
        ctx: &Context<'_>,
        date_range: Option<AnalyticsDateRange>,
    ) -> Result<Vec<AnalyticsCodeIntelByLanguageResult>> {
        let w = window(self.today, date_range, None)?;
        let rows = query::codeintel_by_language(&services(ctx)?.db, &w)
            .await
            .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .map(|r| AnalyticsCodeIntelByLanguageResult {
                language: r.language,
                precision: precision(r.precise).to_owned(),
                count: r.count as f64,
            })
            .collect())
    }

    /// The repositories with the most code navigation events (those the viewer may read).
    async fn code_intel_top_repositories(
        &self,
        ctx: &Context<'_>,
        date_range: Option<AnalyticsDateRange>,
    ) -> Result<Vec<AnalyticsCodeIntelRepositoryResult>> {
        let w = window(self.today, date_range, None)?;
        let db = &services(ctx)?.db;
        let rows = query::codeintel_top_repositories(db, &w, 5)
            .await
            .map_err(db_error)?;
        let mut repos: Vec<String> = rows.iter().map(|r| r.repo_name.clone()).collect();
        repos.sort();
        repos.dedup();
        // Repositories the viewer may not read look like missing ones.
        let access = repo_access(ctx).await?;
        let mut readable = Vec::new();
        for name in repos {
            if db
                .repos()
                .get_by_name_authorized(&name, &access)
                .await
                .map_err(db_error)?
                .is_some()
            {
                readable.push(name);
            }
        }
        let precise = query::repos_with_precise(db, &readable)
            .await
            .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .filter(|r| readable.contains(&r.repo_name))
            .map(|r| AnalyticsCodeIntelRepositoryResult {
                has_precise: precise.contains(&r.repo_name),
                name: r.repo_name,
                language: r.language,
                kind: r.kind,
                precision: precision(r.precise).to_owned(),
                events: r.events as f64,
            })
            .collect())
    }

    /// Repositories, and those with precise code navigation.
    async fn repos(&self, ctx: &Context<'_>) -> Result<AnalyticsReposSummartResult> {
        let (count, precise) = query::repos_summary(&services(ctx)?.db)
            .await
            .map_err(db_error)?;
        Ok(AnalyticsReposSummartResult {
            count: count as f64,
            precise_code_intel_count: precise as f64,
        })
    }

    /// Changesets created and merged by bulk changes.
    async fn batch_changes(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsBatchChangesResult> {
        Ok(AnalyticsBatchChangesResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Searches from the editor extensions and code navigation from the browser extension.
    async fn extensions(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsExtensionsResult> {
        Ok(AnalyticsExtensionsResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Trends.
    async fn code_insights(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsCodesInsightsResult> {
        Ok(AnalyticsCodesInsightsResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// Watches.
    async fn code_monitors(
        &self,
        date_range: Option<AnalyticsDateRange>,
        grouping: Option<AnalyticsGrouping>,
    ) -> Result<AnalyticsCodeMonitorsResult> {
        Ok(AnalyticsCodeMonitorsResult(window(
            self.today, date_range, grouping,
        )?))
    }

    /// When the rollups were last refreshed; null before the first run.
    async fn updated_at(&self, ctx: &Context<'_>) -> Result<Option<DateTime<Utc>>> {
        query::last_rollup(&services(ctx)?.db)
            .await
            .map_err(db_error)
    }
}

fn precision(precise: bool) -> &'static str {
    if precise { "precise" } else { "search-based" }
}

/// Search analytics.
pub struct AnalyticsSearchResult(Window);

#[Object]
impl AnalyticsSearchResult {
    /// Searches run.
    async fn searches(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::SEARCH_RESULTS_QUERIED], &self.0).await
    }

    /// Search result clicks.
    async fn result_clicks(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::SEARCH_RESULT_CLICKED], &self.0).await
    }

    /// File views.
    async fn file_views(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::VIEW_BLOB], &self.0).await
    }

    /// Files opened on the code host.
    async fn file_opens(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::GO_TO_CODE_HOST], &self.0).await
    }

    /// Code copied from results and files.
    async fn code_copied(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::CODE_COPIED], &self.0).await
    }

    /// Searches by type: the result type of a `type:` filter, else the pattern type, most
    /// first.
    async fn searches_by_type(&self, ctx: &Context<'_>) -> Result<Vec<AnalyticsCountItem>> {
        let rows = query::searches_by_type(&services(ctx)?.db, &self.0)
            .await
            .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .map(|(key, count)| AnalyticsCountItem {
                key,
                count: count as f64,
            })
            .collect())
    }

    /// Search latency percentiles, from the time to the last result of streamed searches
    ///. Approximated by latency buckets.
    async fn latency(&self, ctx: &Context<'_>) -> Result<AnalyticsSearchLatency> {
        let h = query::search_latency(&services(ctx)?.db, &self.0)
            .await
            .map_err(db_error)?;
        Ok(AnalyticsSearchLatency {
            p50: names::latency_quantile(&h, 0.5),
            p90: names::latency_quantile(&h, 0.9),
            p99: names::latency_quantile(&h, 0.99),
        })
    }
}

/// A count by key.
#[derive(SimpleObject)]
pub struct AnalyticsCountItem {
    key: String,
    count: f64,
}

/// Search latency percentiles in milliseconds (upper bounds of latency buckets); null
/// without searches.
#[derive(SimpleObject)]
pub struct AnalyticsSearchLatency {
    p50: Option<f64>,
    p90: Option<f64>,
    p99: Option<f64>,
}

/// Notebook analytics.
pub struct AnalyticsNotebooksResults(Window);

#[Object]
impl AnalyticsNotebooksResults {
    async fn creations(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::NOTEBOOK_CREATED], &self.0).await
    }

    async fn views(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::NOTEBOOK_VIEWED], &self.0).await
    }

    async fn block_runs(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::NOTEBOOK_BLOCK_RUN], &self.0).await
    }
}

/// Users by number of days used.
#[derive(SimpleObject)]
pub struct AnalyticsUsersFrequencyItem {
    days_used: f64,
    /// Users.
    frequency: f64,
    /// Share of the active users, in percent.
    percentage: f64,
}

/// Monthly active users.
#[derive(SimpleObject)]
pub struct AnalyticsMonthlyActiveUsers {
    /// The month (`YYYY-MM`).
    date: String,
    count: f64,
}

/// User analytics.
pub struct AnalyticsUsersResult {
    window: Window,
    today: NaiveDate,
}

#[Object]
impl AnalyticsUsersResult {
    /// Activity: every event.
    async fn activity(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        stat(ctx, EventFilter::all(), &self.window).await
    }

    /// Users by number of days they used the instance in the date range.
    async fn frequencies(&self, ctx: &Context<'_>) -> Result<Vec<AnalyticsUsersFrequencyItem>> {
        let rows = query::frequencies(&services(ctx)?.db, &self.window)
            .await
            .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .map(|f| AnalyticsUsersFrequencyItem {
                days_used: f.days_used as f64,
                frequency: f.users as f64,
                percentage: f.percentage,
            })
            .collect())
    }

    /// Active users of the last three months, oldest first.
    async fn monthly_active_users(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<AnalyticsMonthlyActiveUsers>> {
        let mut rows = query::active_users(&services(ctx)?.db, Period::Month, 3, self.today)
            .await
            .map_err(db_error)?;
        rows.reverse();
        Ok(rows
            .into_iter()
            .map(|p| AnalyticsMonthlyActiveUsers {
                date: p.start.format("%Y-%m").to_string(),
                count: p.users as f64,
            })
            .collect())
    }

    /// Sign-ins.
    async fn sign_ins(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::SIGN_IN], &self.window).await
    }
}

/// Code navigation analytics.
pub struct AnalyticsCodeIntelResult(Window);

#[Object]
impl AnalyticsCodeIntelResult {
    /// "Find references" clicks.
    async fn reference_clicks(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::FIND_REFERENCES], &self.0).await
    }

    /// "Go to definition" clicks.
    async fn definition_clicks(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(
            ctx,
            &[names::GO_TO_DEFINITION, names::GO_TO_DEFINITION_PRELOADED],
            &self.0,
        )
        .await
    }

    /// Code navigation requests from the web app and API clients.
    async fn in_app_events(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_events()).except_source(CODE_HOST_INTEGRATION);
        stat(ctx, f, &self.0).await
    }

    /// Code navigation requests from the browser extension.
    async fn code_host_events(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_events()).only_source(CODE_HOST_INTEGRATION);
        stat(ctx, f, &self.0).await
    }

    /// Search-based (and syntactic) code navigation requests.
    async fn search_based_events(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_events_by_precision(false));
        stat(ctx, f, &self.0).await
    }

    /// Precise code navigation requests.
    async fn precise_events(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_events_by_precision(true));
        stat(ctx, f, &self.0).await
    }

    /// Code navigation requests with results in other repositories.
    async fn cross_repo_events(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_cross_repo_events());
        stat(ctx, f, &self.0).await
    }
}

/// Code navigation events of a language and precision.
#[derive(SimpleObject)]
pub struct AnalyticsCodeIntelByLanguageResult {
    language: String,
    /// `precise` or `search-based`.
    precision: String,
    count: f64,
}

/// Code navigation events of a repository, language, kind and precision.
#[derive(SimpleObject)]
pub struct AnalyticsCodeIntelRepositoryResult {
    name: String,
    language: String,
    /// `hover`, `definitions` or `references`.
    kind: String,
    /// `precise` or `search-based`.
    precision: String,
    events: f64,
    /// Whether the repository has a precise index.
    has_precise: bool,
}

/// Repository counts.
#[derive(SimpleObject)]
pub struct AnalyticsReposSummartResult {
    count: f64,
    precise_code_intel_count: f64,
}

/// Bulk changes analytics.
pub struct AnalyticsBatchChangesResult(Window);

#[Object]
impl AnalyticsBatchChangesResult {
    /// Changesets published by bulk changes, by creation date.
    async fn changesets_created(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        Ok(query::changesets_created(&services(ctx)?.db, &self.0)
            .await
            .map_err(db_error)?
            .into())
    }

    /// Changesets of bulk changes merged.
    async fn changesets_merged(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        Ok(query::changesets_merged(&services(ctx)?.db, &self.0)
            .await
            .map_err(db_error)?
            .into())
    }

    /// Bulk change list and detail page views.
    async fn views(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(
            ctx,
            &[names::BATCH_CHANGE_LIST_VIEWED, names::BATCH_CHANGE_VIEWED],
            &self.0,
        )
        .await
    }

    /// Bulk change specs applied.
    async fn applies(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::BATCH_CHANGE_APPLIED], &self.0).await
    }
}

/// Extension analytics.
pub struct AnalyticsExtensionsResult(Window);

#[Object]
impl AnalyticsExtensionsResult {
    /// Searches from the JetBrains plugin.
    async fn jetbrains(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::JETBRAINS_SEARCH], &self.0).await
    }

    /// Searches from the VS Code extension.
    async fn vscode(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::VSCODE_SEARCH], &self.0).await
    }

    /// Code navigation from the browser extension.
    async fn browser(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        let f = EventFilter::names(&names::codeintel_events()).only_source(CODE_HOST_INTEGRATION);
        stat(ctx, f, &self.0).await
    }
}

/// Trends analytics.
pub struct AnalyticsCodesInsightsResult(Window);

#[Object]
impl AnalyticsCodesInsightsResult {
    async fn insight_hovers(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::INSIGHT_HOVER], &self.0).await
    }

    async fn insight_data_point_clicks(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::INSIGHT_DATA_POINT_CLICK], &self.0).await
    }

    /// Trends page views.
    async fn views(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::INSIGHTS_VIEWED], &self.0).await
    }

    /// Trends created.
    async fn creations(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::INSIGHT_ADDED], &self.0).await
    }
}

/// Watch analytics.
pub struct AnalyticsCodeMonitorsResult(Window);

#[Object]
impl AnalyticsCodeMonitorsResult {
    /// Watches created.
    async fn creations(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(ctx, &[names::MONITOR_CREATED], &self.0).await
    }

    /// Watch list and page views.
    async fn views(&self, ctx: &Context<'_>) -> Result<AnalyticsStatItem> {
        named(
            ctx,
            &[names::MONITOR_LIST_VIEWED, names::MONITOR_VIEWED],
            &self.0,
        )
        .await
    }
}

/// Active users of one day, week or month.
#[derive(SimpleObject)]
pub struct SiteUsagePeriod {
    /// The start of the period (RFC 3339).
    start_time: String,
    user_count: i32,
    registered_user_count: i32,
    anonymous_user_count: i32,
    /// Always 0: there is no integration traffic tracked separately.
    integration_user_count: i32,
}

/// DAU, WAU and MAU, newest first.
#[derive(SimpleObject)]
pub struct SiteUsageStatistics {
    daus: Vec<SiteUsagePeriod>,
    waus: Vec<SiteUsagePeriod>,
    maus: Vec<SiteUsagePeriod>,
}

/// `site.usageStatistics`.
pub async fn usage_statistics(
    ctx: &Context<'_>,
    days: Option<i32>,
    weeks: Option<i32>,
    months: Option<i32>,
) -> Result<SiteUsageStatistics> {
    let db = &services(ctx)?.db;
    let today = Utc::now().date_naive();
    let periods = |n: Option<i32>, default: i32, max: i32| {
        usize::try_from(n.unwrap_or(default).clamp(0, max)).unwrap_or(0)
    };
    let mut out = Vec::with_capacity(3);
    for (period, n) in [
        (Period::Day, periods(days, 14, 366)),
        (Period::Week, periods(weeks, 10, 105)),
        (Period::Month, periods(months, 3, 25)),
    ] {
        let rows = query::active_users(db, period, n, today)
            .await
            .map_err(db_error)?;
        out.push(
            rows.into_iter()
                .map(|p| SiteUsagePeriod {
                    start_time: iso(p.start),
                    user_count: crate::context::to_int(p.users),
                    registered_user_count: crate::context::to_int(p.registered_users),
                    anonymous_user_count: crate::context::to_int(p.anonymous_users),
                    integration_user_count: 0,
                })
                .collect(),
        );
    }
    let maus = out.pop().unwrap_or_default();
    let waus = out.pop().unwrap_or_default();
    let daus = out.pop().unwrap_or_default();
    Ok(SiteUsageStatistics { daus, waus, maus })
}

/// A timestamp filter of `site.users`.
#[derive(InputObject, Clone, Copy, Default)]
pub struct SiteUsersDateRangeInput {
    /// Less than or equal to.
    lte: Option<DateTime<Utc>>,
    /// Greater than or equal to.
    gte: Option<DateTime<Utc>>,
    /// Negates the filter.
    not: Option<bool>,
    /// Only null (`true`) or only set (`false`) values.
    empty: Option<bool>,
}

impl From<SiteUsersDateRangeInput> for query::TimeRange {
    fn from(r: SiteUsersDateRangeInput) -> Self {
        Self {
            gte: r.gte,
            lte: r.lte,
            empty: r.empty,
            not: r.not.unwrap_or(false),
        }
    }
}

/// A number filter of `site.users`.
#[derive(InputObject, Clone, Copy, Default)]
pub struct SiteUsersNumberRangeInput {
    gte: Option<f64>,
    lte: Option<f64>,
}

/// The order of `site.users`.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum SiteUserOrderBy {
    Username,
    Email,
    EventsCount,
    LastActiveAt,
    CreatedAt,
    DeletedAt,
    SiteAdmin,
}

impl From<SiteUserOrderBy> for UserOrder {
    fn from(o: SiteUserOrderBy) -> Self {
        match o {
            SiteUserOrderBy::Username => UserOrder::Username,
            SiteUserOrderBy::Email => UserOrder::Email,
            SiteUserOrderBy::EventsCount => UserOrder::EventsCount,
            SiteUserOrderBy::LastActiveAt => UserOrder::LastActiveAt,
            SiteUserOrderBy::CreatedAt => UserOrder::CreatedAt,
            SiteUserOrderBy::DeletedAt => UserOrder::DeletedAt,
            SiteUserOrderBy::SiteAdmin => UserOrder::SiteAdmin,
        }
    }
}

/// The filters of `site.users`.
#[derive(Default)]
pub struct SiteUsersArgs {
    pub query: Option<String>,
    pub site_admin: Option<bool>,
    pub username: Option<String>,
    pub email: Option<String>,
    pub last_active_at: Option<SiteUsersDateRangeInput>,
    pub deleted_at: Option<SiteUsersDateRangeInput>,
    pub created_at: Option<SiteUsersDateRangeInput>,
    pub events_count: Option<SiteUsersNumberRangeInput>,
}

/// Users with their activity (site admins only).
pub struct SiteUsers(query::UserListOptions);

impl SiteUsers {
    /// `site.users` with `args`.
    pub fn new(args: SiteUsersArgs) -> Self {
        let query = [args.query, args.username, args.email]
            .into_iter()
            .flatten()
            .find(|q| !q.trim().is_empty());
        Self(query::UserListOptions {
            query,
            site_admin: args.site_admin,
            last_active_at: args.last_active_at.map(Into::into),
            created_at: args.created_at.map(Into::into),
            deleted_at: args.deleted_at.map(Into::into),
            events_count_gte: args.events_count.and_then(|r| r.gte),
            events_count_lte: args.events_count.and_then(|r| r.lte),
            ..Default::default()
        })
    }
}

#[Object]
impl SiteUsers {
    /// Users matching the filters.
    async fn total_count(&self, ctx: &Context<'_>) -> Result<f64> {
        Ok(query::count_users(&services(ctx)?.db, &self.0)
            .await
            .map_err(db_error)? as f64)
    }

    /// A page of the users.
    async fn nodes(
        &self,
        ctx: &Context<'_>,
        limit: Option<i32>,
        offset: Option<i32>,
        order_by: Option<SiteUserOrderBy>,
        descending: Option<bool>,
    ) -> Result<Vec<SiteUser>> {
        let opts = query::UserListOptions {
            limit: i64::from(limit.unwrap_or(25).clamp(0, 1000)),
            offset: i64::from(offset.unwrap_or(0).max(0)),
            order: order_by.map(Into::into).unwrap_or_default(),
            descending: descending.unwrap_or(false),
            ..self.0.clone()
        };
        let rows = query::list_users(&services(ctx)?.db, &opts)
            .await
            .map_err(db_error)?;
        Ok(rows.into_iter().map(SiteUser).collect())
    }
}

/// A user with activity.
pub struct SiteUser(query::UserActivity);

#[Object]
impl SiteUser {
    /// The `User` node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(USER_KIND, self.0.id))
    }

    async fn username(&self) -> &str {
        &self.0.username
    }

    /// The primary (else the first) email.
    async fn email(&self) -> Option<&str> {
        self.0.email.as_deref()
    }

    async fn display_name(&self) -> Option<&str> {
        self.0.display_name.as_deref()
    }

    async fn created_at(&self) -> String {
        self.0.created_at.to_rfc3339()
    }

    /// The last usage event (raw events, else the rollups).
    async fn last_active_at(&self) -> Option<String> {
        self.0.last_active_at.map(|t| t.to_rfc3339())
    }

    async fn deleted_at(&self) -> Option<String> {
        self.0.deleted_at.map(|t| t.to_rfc3339())
    }

    async fn site_admin(&self) -> bool {
        self.0.site_admin
    }

    #[graphql(name = "scimControlled")]
    async fn scim_controlled(&self) -> bool {
        self.0.scim_controlled
    }

    /// Usage events in the rollups (up to half an hour behind).
    async fn events_count(&self) -> f64 {
        self.0.events_count as f64
    }

    async fn locked(&self) -> bool {
        self.0.locked
    }
}
