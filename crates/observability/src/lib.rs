//! sourcegull-observability: structured logs, Prometheus metrics and per-operation
//! instrumentation.
//!
//! - [`init`] installs the global `tracing` subscriber (JSON by default).
//! - [`install_metrics`] installs the Prometheus recorder; the HTTP layer serves
//!   [`MetricsHandle::render`] on `/metrics`.
//! - [`Operation`] / [`observe`] wrap a unit of work with a span, RED metrics and
//!   error/slow logging.

mod logging;
mod metrics_export;
mod operation;

pub use logging::{LogConfig, LogFormat, init};
pub use metrics_export::{DURATION_BUCKETS, MetricsHandle, build_recorder, install_metrics};
pub use operation::{OP_DURATION_METRIC, OP_TOTAL_METRIC, Operation, observe, observe_sync};

/// Errors returned while setting up observability.
#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    /// The log filter directive (RUST_LOG syntax) could not be parsed.
    #[error("invalid log filter {filter:?}: {message}")]
    InvalidFilter {
        /// The rejected filter string.
        filter: String,
        /// Parser error message.
        message: String,
    },
    /// The Prometheus recorder could not be built or installed.
    #[error("installing metrics recorder: {0}")]
    Metrics(String),
}
