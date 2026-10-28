//! Search over GraphQL: `searchQueryAggregate`, the aggregations
//! behind the search page's bar chart.
//!
//! An aggregation runs the
//! query to the end (`count:all`) for at most [`TIMEOUT`] (or
//! [`EXTENDED_TIMEOUT`] with `extendedTimeout: true`) and groups up to
//! [`MAX_MATCHES`] results ([`sourcegull_search::aggregate`]). A search that
//! times out gives `SearchAggregationNotAvailable` with
//! `TIMEOUT_EXTENSION_AVAILABLE` (or `TIMEOUT_NO_EXTENSION_AVAILABLE` once
//! extended); one that hits the match cap gives a
//! `NonExhaustiveSearchAggregationResult`.

use std::time::Duration;

use async_graphql::{Context, Enum, Object, Result, SimpleObject, Union};
use sourcegull_query::PatternType;
use sourcegull_search::aggregate::{AggregationMode, Aggregator, QueryShape};

use crate::context::{codes, error, services};
use crate::repos::require_reader;

/// How long an aggregation's search may run.
pub const TIMEOUT: Duration = Duration::from_secs(2);
pub const EXTENDED_TIMEOUT: Duration = Duration::from_secs(55);
/// Results an aggregation counts at most.
pub const MAX_MATCHES: usize = 100_000;

#[derive(Default)]
pub struct SearchQuery;

#[Object]
impl SearchQuery {
    /// Aggregations of a query's results.
    async fn search_query_aggregate(
        &self,
        ctx: &Context<'_>,
        query: String,
        pattern_type: SearchPatternType,
    ) -> Result<SearchQueryAggregate> {
        require_reader(ctx)?;
        Ok(SearchQueryAggregate {
            query,
            pattern_type: pattern_type.into(),
        })
    }
}

pub use sourcegull_graphql_core::search::SearchPatternType;

fn pattern_type_name(t: PatternType) -> &'static str {
    match t {
        PatternType::Keyword => "keyword",
        PatternType::Standard => "standard",
        PatternType::Literal => "literal",
        PatternType::Regexp => "regexp",
        PatternType::Structural => "structural",
    }
}

#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum SearchAggregationMode {
    Repo,
    Path,
    Author,
    CaptureGroup,
}

impl From<SearchAggregationMode> for AggregationMode {
    fn from(m: SearchAggregationMode) -> AggregationMode {
        match m {
            SearchAggregationMode::Repo => AggregationMode::Repo,
            SearchAggregationMode::Path => AggregationMode::Path,
            SearchAggregationMode::Author => AggregationMode::Author,
            SearchAggregationMode::CaptureGroup => AggregationMode::CaptureGroup,
        }
    }
}

impl From<AggregationMode> for SearchAggregationMode {
    fn from(m: AggregationMode) -> SearchAggregationMode {
        match m {
            AggregationMode::Repo => SearchAggregationMode::Repo,
            AggregationMode::Path => SearchAggregationMode::Path,
            AggregationMode::Author => SearchAggregationMode::Author,
            AggregationMode::CaptureGroup => SearchAggregationMode::CaptureGroup,
        }
    }
}

#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "NotAvailableReasonType")]
pub enum NotAvailableReasonType {
    InvalidQuery,
    InvalidAggregationModeForQuery,
    TimeoutExtensionAvailable,
    TimeoutNoExtensionAvailable,
    ErrorOccurred,
}

pub struct SearchQueryAggregate {
    query: String,
    pattern_type: PatternType,
}

#[derive(SimpleObject)]
pub struct AggregationModeAvailability {
    mode: SearchAggregationMode,
    available: bool,
    reason_unavailable: Option<String>,
}

#[derive(SimpleObject)]
pub struct AggregationGroup {
    label: String,
    count: i32,
    query: Option<String>,
}

#[derive(SimpleObject)]
pub struct ExhaustiveSearchAggregationResult {
    groups: Vec<AggregationGroup>,
    other_result_count: Option<i32>,
    other_group_count: Option<i32>,
    supports_persistence: Option<bool>,
    mode: SearchAggregationMode,
}

#[derive(SimpleObject)]
pub struct NonExhaustiveSearchAggregationResult {
    groups: Vec<AggregationGroup>,
    other_result_count: Option<i32>,
    approximate_other_group_count: Option<i32>,
    supports_persistence: Option<bool>,
    mode: SearchAggregationMode,
}

#[derive(SimpleObject)]
pub struct SearchAggregationNotAvailable {
    reason: String,
    reason_type: NotAvailableReasonType,
    mode: SearchAggregationMode,
}

#[derive(Union)]
pub enum SearchAggregationResult {
    Exhaustive(ExhaustiveSearchAggregationResult),
    NonExhaustive(NonExhaustiveSearchAggregationResult),
    NotAvailable(SearchAggregationNotAvailable),
}

fn int(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn not_available(
    mode: SearchAggregationMode,
    reason_type: NotAvailableReasonType,
    reason: impl Into<String>,
) -> SearchAggregationResult {
    SearchAggregationResult::NotAvailable(SearchAggregationNotAvailable {
        reason: reason.into(),
        reason_type,
        mode,
    })
}

impl SearchQueryAggregate {
    fn shape(&self) -> std::result::Result<QueryShape, String> {
        QueryShape::new(&self.query, self.pattern_type, chrono::Utc::now())
    }
}

#[Object]
impl SearchQueryAggregate {
    /// The modes this query supports.
    async fn mode_availability(&self) -> Vec<AggregationModeAvailability> {
        let shape = self.shape();
        AggregationMode::ALL
            .into_iter()
            .map(|m| {
                let check = match &shape {
                    Ok(s) => s.check(m).map_err(str::to_owned),
                    Err(e) => Err(e.clone()),
                };
                AggregationModeAvailability {
                    mode: m.into(),
                    available: check.is_ok(),
                    reason_unavailable: check.err(),
                }
            })
            .collect()
    }

    /// The query's results grouped by `mode` (by default the first of
    /// capture group, author and repository that applies), the `limit`
    /// largest groups.
    async fn aggregations(
        &self,
        ctx: &Context<'_>,
        mode: Option<SearchAggregationMode>,
        limit: i32,
        #[graphql(default = false)] extended_timeout: bool,
    ) -> Result<SearchAggregationResult> {
        let shape = match self.shape() {
            Ok(s) => s,
            Err(e) => {
                let mode = mode.unwrap_or(SearchAggregationMode::Repo);
                return Ok(not_available(mode, NotAvailableReasonType::InvalidQuery, e));
            }
        };
        let m: AggregationMode = mode.map_or_else(|| shape.default_mode(), Into::into);
        if let Err(reason) = shape.check(m) {
            return Ok(not_available(
                m.into(),
                NotAvailableReasonType::InvalidAggregationModeForQuery,
                reason,
            ));
        }
        let timeout = if extended_timeout {
            EXTENDED_TIMEOUT
        } else {
            TIMEOUT
        };
        let query = format!("{} count:all timeout:{}s", self.query, timeout.as_secs());
        let state = services(ctx)?
            .app
            .as_ref()
            .ok_or_else(|| error(codes::INTERNAL, "Search is not available."))?;
        let out = sourcegull_search_web::collect(
            &state.search_backend(),
            crate::context::viewer(ctx),
            &query,
            Some(pattern_type_name(self.pattern_type)),
            MAX_MATCHES,
        )
        .await;
        if let Some(e) = out.error {
            tracing::error!(error = %e, "aggregation search failed");
            return Ok(not_available(
                m.into(),
                NotAvailableReasonType::ErrorOccurred,
                "The search failed.",
            ));
        }
        if out.progress.is_none() {
            let reason = out.alerts.first().map_or_else(
                || "The query was rejected.".to_owned(),
                |a| a.description.clone(),
            );
            return Ok(not_available(
                m.into(),
                NotAvailableReasonType::InvalidQuery,
                reason,
            ));
        }
        if out.timed_out() {
            return Ok(if extended_timeout {
                not_available(
                    m.into(),
                    NotAvailableReasonType::TimeoutNoExtensionAvailable,
                    "The search timed out.",
                )
            } else {
                not_available(
                    m.into(),
                    NotAvailableReasonType::TimeoutExtensionAvailable,
                    "The search timed out; run it longer to see the aggregation.",
                )
            });
        }
        let limit = usize::try_from(limit)
            .map_err(|_| error(codes::INVALID_INPUT, "limit must not be negative."))?;
        let limit_hit = out.limit_hit();
        let mut agg = Aggregator::new(shape, m);
        out.matches.iter().for_each(|r| agg.add(r));
        let agg = agg.finish(limit);
        let groups = agg
            .groups
            .into_iter()
            .map(|g| AggregationGroup {
                label: g.label,
                count: int(g.count),
                query: Some(g.query),
            })
            .collect();
        Ok(if limit_hit {
            SearchAggregationResult::NonExhaustive(NonExhaustiveSearchAggregationResult {
                groups,
                other_result_count: Some(int(agg.other_result_count)),
                approximate_other_group_count: Some(int(agg.other_group_count)),
                supports_persistence: Some(false),
                mode: m.into(),
            })
        } else {
            SearchAggregationResult::Exhaustive(ExhaustiveSearchAggregationResult {
                groups,
                other_result_count: Some(int(agg.other_result_count)),
                other_group_count: Some(int(agg.other_group_count)),
                supports_persistence: Some(false),
                mode: m.into(),
            })
        })
    }
}
