//! Prometheus metrics recorder.

use std::sync::Mutex;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::ObservabilityError;

/// Histogram buckets (seconds) used for every metric whose name ends in `_seconds`.
///
/// Spans 1 ms to 60 s, which covers in-memory index lookups up to slow
/// code-host calls and clones.
pub const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

static HANDLE: Mutex<Option<MetricsHandle>> = Mutex::new(None);

/// Handle to the installed Prometheus recorder.
#[derive(Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsHandle").finish_non_exhaustive()
    }
}

impl MetricsHandle {
    /// Renders all metrics in the Prometheus text exposition format, for `/metrics`.
    pub fn render(&self) -> String {
        // The exporter needs periodic upkeep to drain histogram buffers; doing
        // it on scrape keeps this crate free of background tasks.
        self.inner.run_upkeep();
        self.inner.render()
    }

    /// Wraps an existing exporter handle (e.g. one built with a local recorder in tests).
    pub fn from_prometheus(inner: PrometheusHandle) -> Self {
        Self { inner }
    }
}

/// Builds a recorder configured with Sourcegull's histogram buckets, without installing it.
pub fn build_recorder()
-> Result<metrics_exporter_prometheus::PrometheusRecorder, ObservabilityError> {
    Ok(builder()?.build_recorder())
}

fn builder() -> Result<PrometheusBuilder, ObservabilityError> {
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".to_owned()), DURATION_BUCKETS)
        .map_err(|e| ObservabilityError::Metrics(e.to_string()))
}

/// Installs the global Prometheus recorder and returns a handle for rendering.
///
/// Idempotent: later calls return the handle from the first successful call.
/// Fails if a different global `metrics` recorder was installed elsewhere.
pub fn install_metrics() -> Result<MetricsHandle, ObservabilityError> {
    let mut slot = HANDLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handle) = slot.as_ref() {
        return Ok(handle.clone());
    }
    let recorder = builder()?.build_recorder();
    let handle = MetricsHandle::from_prometheus(recorder.handle());
    metrics::set_global_recorder(recorder)
        .map_err(|e| ObservabilityError::Metrics(e.to_string()))?;
    *slot = Some(handle.clone());
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_renders() {
        let first = install_metrics().expect("install");
        let second = install_metrics().expect("second install");
        metrics::counter!("sourcegull_test_installed_total").increment(3);
        let out = second.render();
        assert!(out.contains("sourcegull_test_installed_total 3"), "{out}");
        // Other tests in this binary record metrics concurrently, so compare the shared
        // recorder through a series of our own rather than whole renders.
        assert!(first.render().contains("sourcegull_test_installed_total 3"));
    }
}
