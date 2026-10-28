//! Usage analytics, computed from the local `event_logs` only. Nothing leaves the instance; there
//! are no pings.
//!
//! - [`names`]: the event names the server and the web app log, and the groups the admin
//!   analytics count.
//! - [`rollup`]: the worker's leader-elected job. It aggregates `event_logs` into daily
//!   rollup tables (events per day, name, source and user; active users per day, week and
//!   month; searches by type and latency; code navigation by repository and language),
//!   idempotently (a day is recomputed as a whole), then prunes raw events past
//!   `eventLogs.retention` and rollups past `eventLogs.rollupRetention`.
//! - [`query`]: the admin analytics over the rollups: time series by day or week with event
//!   counts, unique and registered users; DAU/WAU/MAU; per-user activity.

mod feature;
pub mod names;
pub mod query;
pub mod rollup;
pub use feature::AnalyticsFeature;

pub use rollup::{ROLLUP, ROLLUP_INTERVAL, RollupStats, prune, rollup, run_rollup};
