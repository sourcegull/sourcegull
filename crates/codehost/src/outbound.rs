//! The outbound request log: code host API calls, for site admins debugging a
//! connection.
//!
//! Clients call [`record`] after each HTTP attempt. Nothing is logged until startup installs a
//! sink with [`set_global`]: [`MemoryOutboundLog`] keeps a ring in this process, and the
//! Postgres sink in `sourcegull-repos` shares one ring (bounded by the site setting
//! `outboundRequestLogLimit`) between every process.
//!
//! Entries never hold secrets: [`redact_url`] blanks credentials in URLs (user info and
//! token-like query parameters) and [`redact_headers`] the values of auth headers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use url::Url;

/// The value that replaces a secret.
pub const REDACTED: &str = "REDACTED";

/// Longest request body kept (bytes); longer bodies are truncated.
pub const MAX_LOGGED_BODY: usize = 2048;

/// One outbound HTTP request (one attempt, so retries are separate entries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundRequestEntry {
    pub started_at: DateTime<Utc>,
    pub method: String,
    /// The URL, redacted ([`redact_url`]).
    pub url: String,
    /// Header names and values, redacted ([`redact_headers`]).
    pub request_headers: Vec<(String, Vec<String>)>,
    /// The request body, truncated to [`MAX_LOGGED_BODY`].
    pub request_body: String,
    /// 0 when no response arrived.
    pub status_code: u16,
    pub response_headers: Vec<(String, Vec<String>)>,
    pub duration: Duration,
    /// The transport error, empty on a response.
    pub error: String,
    /// Which client sent it, e.g. `gitlab https://gitlab.example.com/`.
    pub client: String,
    /// Where in Sourcegull the request was made (best effort; may be empty).
    pub caller: String,
}

/// Where outbound requests go.
pub trait OutboundRequestLog: Send + Sync {
    /// Logs `entry`. Must not block: called inline by API clients.
    fn record(&self, entry: OutboundRequestEntry);
}

fn global_slot() -> &'static RwLock<Option<Arc<dyn OutboundRequestLog>>> {
    static GLOBAL: OnceLock<RwLock<Option<Arc<dyn OutboundRequestLog>>>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(None))
}

/// Installs the process-wide log (at startup).
pub fn set_global(log: Arc<dyn OutboundRequestLog>) {
    *global_slot().write().unwrap_or_else(|e| e.into_inner()) = Some(log);
}

/// Whether a log is installed (clients skip building entries otherwise).
pub fn enabled() -> bool {
    global_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Sends `entry` to the installed log, if any.
pub fn record(entry: OutboundRequestEntry) {
    let log = global_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(log) = log {
        log.record(entry);
    }
}

/// Query parameter and header name fragments that mark a secret (compared lowercase).
const SECRET_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "auth",
    "cookie",
    "key",
    "signature",
    "sig",
    "code",
    "credential",
    "sudo",
];

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_MARKERS.iter().any(|m| n.contains(m))
}

/// `url` without secrets: the password (and user name) of the user info and the values of
/// query parameters whose names look like credentials (`private_token`, `access_token`,
/// `client_secret`, `password`, …) become [`REDACTED`].
pub fn redact_url(url: &Url) -> String {
    let mut u = url.clone();
    if !u.username().is_empty() {
        let _ = u.set_username(REDACTED);
    }
    if u.password().is_some() {
        let _ = u.set_password(Some(REDACTED));
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                let v = if is_secret_name(&k) {
                    REDACTED.to_owned()
                } else {
                    v.into_owned()
                };
                (k.into_owned(), v)
            })
            .collect();
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    u.set_fragment(None);
    u.to_string()
}

/// Groups `headers` by name (lowercase) with secret values (`Authorization`, `PRIVATE-TOKEN`,
/// `JOB-TOKEN`, cookies, …) replaced by [`REDACTED`].
pub fn redact_headers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let value = if is_secret_name(&name) && !is_harmless_header(&name) {
            REDACTED.to_owned()
        } else {
            String::from_utf8_lossy(value).into_owned()
        };
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some((_, values)) => values.push(value),
            None => out.push((name, vec![value])),
        }
    }
    out
}

/// Headers whose names match a marker but carry no secret (GitLab's rate limit headers).
fn is_harmless_header(name: &str) -> bool {
    name.starts_with("ratelimit-")
        || matches!(
            name,
            "content-encoding" | "accept-encoding" | "x-content-type-options"
        )
}

/// `body` cut to [`MAX_LOGGED_BODY`] bytes (on a character boundary).
pub fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_LOGGED_BODY {
        return body.to_owned();
    }
    let mut end = MAX_LOGGED_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated, {} bytes)", &body[..end], body.len())
}

/// An in-process ring of the newest entries (one process only).
#[derive(Debug)]
pub struct MemoryOutboundLog {
    limit: AtomicUsize,
    entries: Mutex<VecDeque<OutboundRequestEntry>>,
}

impl MemoryOutboundLog {
    /// A ring of at most `limit` entries (0 logs nothing).
    pub fn new(limit: usize) -> Self {
        Self {
            limit: AtomicUsize::new(limit),
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Changes the bound (dropping the oldest entries if needed).
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
        let mut e = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        e.truncate(limit);
    }

    /// The entries, newest first.
    pub fn entries(&self) -> Vec<OutboundRequestEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

impl OutboundRequestLog for MemoryOutboundLog {
    fn record(&self, entry: OutboundRequestEntry) {
        let limit = self.limit.load(Ordering::Relaxed);
        if limit == 0 {
            return;
        }
        let mut e = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        e.push_front(entry);
        e.truncate(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_credentials() {
        let u = Url::parse(
            "https://oauth2:s3cr3t@gitlab.example.com/api/v4/projects?private_token=abc\
             &per_page=100&access_token=xyz&client_secret=q&page=2#frag",
        )
        .unwrap();
        let r = redact_url(&u);
        for secret in ["s3cr3t", "oauth2", "abc", "xyz", "=q&", "frag"] {
            assert!(!r.contains(secret), "{secret} in {r}");
        }
        assert!(r.contains("per_page=100"), "{r}");
        assert!(r.contains("page=2"), "{r}");
        assert!(r.contains("private_token=REDACTED"), "{r}");
        assert!(r.starts_with("https://REDACTED:REDACTED@gitlab.example.com/api/v4/projects"));
        // Nothing to redact: unchanged.
        let plain = Url::parse("https://gitlab.com/api/v4/version").unwrap();
        assert_eq!(redact_url(&plain), "https://gitlab.com/api/v4/version");
    }

    #[test]
    fn headers_lose_secrets() {
        let h = redact_headers([
            ("PRIVATE-TOKEN", b"glpat-123".as_slice()),
            ("Authorization", b"Bearer abc".as_slice()),
            ("JOB-TOKEN", b"j".as_slice()),
            ("Set-Cookie", b"a=b".as_slice()),
            ("Content-Type", b"application/json".as_slice()),
            ("RateLimit-Remaining", b"10".as_slice()),
            ("Link", b"<x>".as_slice()),
            ("Link", b"<y>".as_slice()),
        ]);
        let get = |n: &str| h.iter().find(|(k, _)| k == n).unwrap().1.clone();
        assert_eq!(get("private-token"), vec![REDACTED]);
        assert_eq!(get("authorization"), vec![REDACTED]);
        assert_eq!(get("job-token"), vec![REDACTED]);
        assert_eq!(get("set-cookie"), vec![REDACTED]);
        assert_eq!(get("content-type"), vec!["application/json"]);
        assert_eq!(get("ratelimit-remaining"), vec!["10"]);
        assert_eq!(get("link"), vec!["<x>", "<y>"]);
    }

    #[test]
    fn memory_ring_is_bounded() {
        let log = MemoryOutboundLog::new(2);
        for i in 0..3 {
            log.record(OutboundRequestEntry {
                url: format!("u{i}"),
                ..Default::default()
            });
        }
        let urls: Vec<_> = log.entries().into_iter().map(|e| e.url).collect();
        assert_eq!(urls, ["u2", "u1"]);
        log.set_limit(0);
        log.record(OutboundRequestEntry::default());
        assert!(log.entries().is_empty());
    }

    #[test]
    fn bodies_are_truncated() {
        assert_eq!(truncate_body("short"), "short");
        let long = "é".repeat(MAX_LOGGED_BODY);
        assert!(truncate_body(&long).contains("truncated"));
    }
}
