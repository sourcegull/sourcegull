//! Per-operation instrumentation: a span, RED metrics and error/slow logs.
//!
//! A small value type rather than an attribute macro, so no proc-macro crate is needed:
//!
//! ```
//! use sourcegull_observability::Operation;
//! use std::time::Duration;
//!
//! static SHARD_SEARCH: Operation =
//!     Operation::new("search.indexed.shard").with_slow_threshold(Duration::from_millis(500));
//!
//! # async fn demo() -> Result<u32, std::io::Error> {
//! let hits = SHARD_SEARCH.run(async { Ok::<_, std::io::Error>(42) }).await?;
//! # Ok(hits) }
//! ```

use std::fmt::Display;
use std::future::IntoFuture;
use std::time::{Duration, Instant};

use sourcegull_types::metric_names;
use tracing::{Instrument, Span};

/// Histogram of operation durations in seconds, labelled `op`.
pub const OP_DURATION_METRIC: &str = metric_names::OP_DURATION_SECONDS;
/// Counter of completed operations, labelled `op` and `outcome` (`ok` | `error`).
pub const OP_TOTAL_METRIC: &str = metric_names::OP_TOTAL;

/// A named, instrumented unit of work. Cheap to copy; usually a `static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    name: &'static str,
    slow_threshold: Option<Duration>,
}

impl Operation {
    /// Creates an operation. Names are dotted paths, e.g. `search.indexed.shard`.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            slow_threshold: None,
        }
    }

    /// Logs successful runs at `info` when they take longer than `threshold`.
    pub const fn with_slow_threshold(mut self, threshold: Duration) -> Self {
        self.slow_threshold = Some(threshold);
        self
    }

    /// The operation name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Runs `fut` inside the operation's span and records metrics and logs for its result.
    pub async fn run<F, T, E>(&self, fut: F) -> Result<T, E>
    where
        F: IntoFuture<Output = Result<T, E>>,
        E: Display,
    {
        let span = self.span();
        let start = Instant::now();
        let result = fut.into_future().instrument(span.clone()).await;
        span.in_scope(|| self.record(start.elapsed(), &result));
        result
    }

    /// Synchronous variant of [`Operation::run`].
    pub fn run_sync<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, E>
    where
        E: Display,
    {
        let span = self.span();
        let _entered = span.enter();
        let start = Instant::now();
        let result = f();
        self.record(start.elapsed(), &result);
        result
    }

    fn span(&self) -> Span {
        // Span names must be static in `tracing`; `otel.name` renames the span
        // when exported through OpenTelemetry.
        tracing::info_span!("op", op = self.name, otel.name = self.name)
    }

    fn record<T, E: Display>(&self, elapsed: Duration, result: &Result<T, E>) {
        let outcome = if result.is_ok() { "ok" } else { "error" };
        metrics::histogram!(OP_DURATION_METRIC, "op" => self.name).record(elapsed.as_secs_f64());
        metrics::counter!(OP_TOTAL_METRIC, "op" => self.name, "outcome" => outcome).increment(1);
        let duration_ms = elapsed.as_secs_f64() * 1000.0;
        match result {
            Err(e) => tracing::warn!(op = self.name, duration_ms, error = %e, "operation failed"),
            Ok(_) => {
                if let Some(threshold) = self.slow_threshold.filter(|t| elapsed > *t) {
                    tracing::info!(
                        op = self.name,
                        duration_ms,
                        threshold_ms = threshold.as_millis() as u64,
                        "slow operation"
                    );
                }
            }
        }
    }
}

/// Runs `fut` as the operation `name` (see [`Operation::run`]).
pub async fn observe<F, T, E>(name: &'static str, fut: F) -> Result<T, E>
where
    F: IntoFuture<Output = Result<T, E>>,
    E: Display,
{
    Operation::new(name).run(fut).await
}

/// Runs `f` as the operation `name` (see [`Operation::run_sync`]).
pub fn observe_sync<T, E: Display>(
    name: &'static str,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    Operation::new(name).run_sync(f)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::metrics_export::build_recorder;

    fn render(f: impl FnOnce()) -> String {
        let recorder = build_recorder().expect("recorder");
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.run_upkeep();
        handle.render()
    }

    #[test]
    fn sync_op_records_metrics() {
        let out = render(|| {
            let op = Operation::new("test.sync");
            assert_eq!(op.run_sync(|| Ok::<_, String>(1)), Ok(1));
            assert!(op.run_sync(|| Err::<(), _>("boom".to_owned())).is_err());
            let _ = observe_sync("test.sync", || Ok::<_, String>(()));
        });
        assert!(
            out.contains(r#"sourcegull_op_total{op="test.sync",outcome="ok"} 2"#),
            "{out}"
        );
        assert!(
            out.contains(r#"sourcegull_op_total{op="test.sync",outcome="error"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"sourcegull_op_duration_seconds_count{op="test.sync"} 3"#),
            "{out}"
        );
        assert!(
            out.contains(r#"sourcegull_op_duration_seconds_bucket{op="test.sync",le="0.001"}"#),
            "histogram buckets expected: {out}"
        );
    }

    #[test]
    fn async_op_records_metrics() {
        let out = render(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let op = Operation::new("test.async").with_slow_threshold(Duration::ZERO);
                let v = op
                    .run(async {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        Ok::<_, std::io::Error>(7)
                    })
                    .await;
                assert_eq!(v.ok(), Some(7));
                let e = observe("test.async", async {
                    Err::<(), _>(std::io::Error::other("nope"))
                })
                .await;
                assert!(e.is_err());
            });
        });
        assert!(
            out.contains(r#"sourcegull_op_total{op="test.async",outcome="ok"} 1"#),
            "{out}"
        );
        assert!(
            out.contains(r#"sourcegull_op_total{op="test.async",outcome="error"} 1"#),
            "{out}"
        );
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn errors_and_slow_runs_are_logged() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = Operation::new("test.log").run_sync(|| Err::<(), _>("disk on fire"));
            let _ = Operation::new("test.slow")
                .with_slow_threshold(Duration::ZERO)
                .run_sync(|| {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok::<_, String>(())
                });
            let _ = Operation::new("test.fast")
                .with_slow_threshold(Duration::from_secs(60))
                .run_sync(|| Ok::<_, String>(()));
        });
        let logs = String::from_utf8(capture.0.lock().expect("lock").clone()).expect("utf8");
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("operation failed"), "{logs}");
        assert!(logs.contains("disk on fire"), "{logs}");
        assert!(logs.contains("slow operation"), "{logs}");
        assert!(logs.contains("test.slow"), "{logs}");
        assert!(!logs.contains("test.fast"), "{logs}");
    }
}
