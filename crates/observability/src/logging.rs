//! Global `tracing` subscriber setup.

use std::fmt::Write as _;

use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{Format, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::ObservabilityError;

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line (the default; for production).
    #[default]
    Json,
    /// Human-readable multi-line output (for development).
    Pretty,
}

/// Logging configuration passed to [`init`].
#[derive(Debug, Clone)]
pub struct LogConfig<'a> {
    /// Output format.
    pub format: LogFormat,
    /// Filter directives in `RUST_LOG` syntax, e.g. `info,sourcegull_db=debug`.
    pub filter: String,
    /// Service/role name added to every JSON log line as `"service"`.
    pub service: &'a str,
}

impl Default for LogConfig<'static> {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            filter: "info".to_owned(),
            service: "sourcegull",
        }
    }
}

/// Installs the global `tracing` subscriber.
///
/// Idempotent: returns `Ok(true)` when this call installed the subscriber and
/// `Ok(false)` when one was already installed (the new config is then ignored).
/// An unparsable filter is an error.
pub fn init(config: &LogConfig<'_>) -> Result<bool, ObservabilityError> {
    let filter =
        EnvFilter::try_new(&config.filter).map_err(|e| ObservabilityError::InvalidFilter {
            filter: config.filter.clone(),
            message: e.to_string(),
        })?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = match config.format {
        LogFormat::Json => builder
            .fmt_fields(JsonFields::new())
            .event_format(json_format(config.service))
            .try_init(),
        LogFormat::Pretty => builder.pretty().try_init(),
    };
    Ok(result.is_ok())
}

fn json_format(service: &str) -> WithService {
    WithService {
        inner: tracing_subscriber::fmt::format()
            .json()
            .with_current_span(true)
            .with_span_list(false),
        service_field: service_field(service),
    }
}

/// Builds the `"service":"<name>",` JSON fragment once, at init time.
fn service_field(service: &str) -> String {
    let mut out = String::from("\"service\":\"");
    for c in service.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push_str("\",");
    out
}

/// JSON event formatter that injects a constant `service` field.
struct WithService {
    inner: Format<Json>,
    service_field: String,
}

impl<S, N> FormatEvent<S, N> for WithService
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let mut buf = String::with_capacity(256);
        self.inner.format_event(ctx, Writer::new(&mut buf), event)?;
        match buf.strip_prefix('{') {
            Some(rest) => {
                writer.write_char('{')?;
                writer.write_str(&self.service_field)?;
                writer.write_str(rest)
            }
            None => writer.write_str(&buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        let cfg = LogConfig {
            filter: "warn".into(),
            ..LogConfig::default()
        };
        let first = init(&cfg).expect("first init");
        let second = init(&cfg).expect("second init");
        assert!(!second, "second init must not install again");
        // `first` may be false if another test installed a subscriber first.
        let _ = first;
        tracing::warn!("logging after double init");
    }

    #[test]
    fn invalid_filter_is_rejected() {
        let cfg = LogConfig {
            filter: "foo=notalevel".into(),
            ..LogConfig::default()
        };
        assert!(matches!(
            init(&cfg),
            Err(ObservabilityError::InvalidFilter { .. })
        ));
    }

    #[test]
    fn json_lines_carry_service() {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .fmt_fields(JsonFields::new())
            .event_format(json_format("repo"))
            .with_writer(move || Sink(sink.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(answer = 42, "hello");
        });
        let out = String::from_utf8(buf.lock().expect("lock").clone()).expect("utf8");
        let line = out.lines().next().expect("one line");
        assert!(line.starts_with(r#"{"service":"repo","#), "{line}");
        assert!(line.contains(r#""message":"hello""#), "{line}");
        assert!(line.contains(r#""answer":42"#), "{line}");
    }

    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn service_field_is_escaped() {
        assert_eq!(service_field("web"), "\"service\":\"web\",");
        assert_eq!(service_field("a\"b\\"), "\"service\":\"a\\\"b\\\\\",");
    }
}
