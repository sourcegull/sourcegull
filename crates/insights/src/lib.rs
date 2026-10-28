//! Trends, the engine: series over time of search match counts,
//! captured values or grouped counts, backfilled and recorded in the background.
//!
//! - [`series`]: definitions (generation method, group, repository scope, sample interval
//!   and its point grid).
//! - [`compile`]: a series query planned like a search and compiled into the spec repo
//!   nodes count (`sourcegull_search_exec::series`).
//! - [`scope`]: the repositories a series counts.
//! - [`store`]: the `insight_*` tables: series, points per repository (partitioned by
//!   year) and pre-summed, per-repository state, and the job queues.
//! - [`jobs`]: the backfiller and the recorder (pgqueue workers, rate-limited).
//! - [`enqueuer`]: the leader-elected loop that starts backfills, completes them and queues
//!   recordings; [`janitor`]: retention and pruning.
//! - [`read`]: points for a viewer, from the aggregate when the viewer can read every
//!   repository of the series, else summed over the readable ones.
//! - [`admin`]: the admin backfill queue (list, retry, move to the front or back).
//! - [`access`]: who may see and change views, dashboards and the backfill queue.
//!
//! The backfill is the plan's redesign: instead of one search per point and repository, each
//! repository's node counts the oldest point in full and every later point from the previous
//! one through a tree diff, so the cost follows the repository's size plus its churn. The
//! dashboards, views and the rest of the GraphQL API build on this crate.

pub mod access;
pub mod admin;
pub mod compile;
pub mod drilldown;
pub mod enqueuer;
pub mod export;
mod feature;
pub mod filters;
pub mod http;
pub mod janitor;
pub mod jobs;
pub mod langstats;
pub mod preview;
pub mod read;
pub mod scope;
pub mod series;
pub mod store;
pub mod views;

pub use compile::{CompiledQuery, compile};
pub use enqueuer::{ENQUEUER, Enqueuer, run_enqueuer};
pub use feature::InsightsFeature;
pub use janitor::{JANITOR, run_janitor};
pub use jobs::{Counter, RateLimiter, RecordReport, workers};
pub use read::{ReadPath, SeriesPoints, points, points_in};
pub use roaring::RoaringBitmap;
pub use series::{GenerationMethod, GroupBy, Interval, IntervalUnit, RepoScope};
pub use store::{
    BackfillState, InsightStore, NewSeries, OTHER, PointValue, Progress, RepoState, Series,
};

/// Checks a new series before it is stored; the error is the message for the user.
pub fn validate(series: &NewSeries) -> Result<(), String> {
    compile(
        &series.query,
        series.method,
        series.group_by,
        chrono::Utc::now(),
    )?;
    match &series.scope {
        RepoScope::Query(q) => {
            scope::criteria_filters(q)?;
        }
        RepoScope::List(names) if names.is_empty() => {
            return Err("List at least one repository.".into());
        }
        _ => {}
    }
    Ok(())
}
