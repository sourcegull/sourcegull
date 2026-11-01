//! `site.updateCheck`: the newest Sourcegull release from the GitHub releases API, for the
//! site admin overview. The answer is kept in this process for [`FRESH_FOR`] (a failure for
//! [`RETRY_AFTER`]), so the API is asked at most a few times a day per process. The request
//! carries only a `User-Agent` with this version; it is recorded in the outbound request log.
//! The `updateCheck` site setting turns it off.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use async_graphql::SimpleObject;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sourcegull_codehost::outbound::{self, OutboundRequestEntry};
use tokio::sync::Mutex;

use crate::Services;

/// Where the newest release is read from.
pub const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/sourcegull/sourcegull/releases/latest";

/// The releases page; a version's notes are at `<RELEASES_URL>/tag/v<version>`.
pub const RELEASES_URL: &str = "https://github.com/sourcegull/sourcegull/releases";

/// How long a successful answer is reused.
const FRESH_FOR: Duration = Duration::from_secs(12 * 3600);
/// How long a failed check is reused before asking again.
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// Time limit of one request.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The result of the release check.
#[derive(SimpleObject, Clone, Debug, PartialEq)]
pub struct UpdateCheck {
    /// Whether checking is enabled (the `updateCheck` site setting).
    enabled: bool,
    /// The running version.
    current_version: String,
    /// The release notes of the running version.
    #[graphql(name = "currentVersionURL")]
    current_version_url: String,
    /// The newest release, e.g. `0.4.0`; null when off or the check failed.
    latest_version: Option<String>,
    /// The release notes of `latestVersion`.
    #[graphql(name = "latestVersionURL")]
    latest_version_url: Option<String>,
    /// Whether `latestVersion` is newer than the running version.
    update_available: bool,
    /// When GitHub was last asked; null when off.
    checked_at: Option<DateTime<Utc>>,
    /// Why the check failed, if it did.
    error_message: Option<String>,
}

/// The URL of the release notes of `version`.
pub fn release_url(version: &str) -> String {
    format!("{RELEASES_URL}/tag/v{}", version.trim_start_matches('v'))
}

/// `major.minor.patch` of a version such as `v0.3.0` or `0.3.0-rc.1` (the suffix is dropped).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (
        parts.next()??,
        parts.next()??,
        parts.next().unwrap_or(Some(0))?,
    );
    parts.next().is_none().then_some(version)
}

/// Whether `latest` is a newer version than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

struct Cached {
    at: Instant,
    checked_at: DateTime<Utc>,
    result: Result<String, String>,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The check, from the cache when it is fresh. Concurrent callers wait for one request.
pub async fn check(svc: &Services) -> UpdateCheck {
    let current = env!("CARGO_PKG_VERSION");
    let mut out = UpdateCheck {
        enabled: svc.config.load().value.update_check,
        current_version: current.to_owned(),
        current_version_url: release_url(current),
        latest_version: None,
        latest_version_url: None,
        update_available: false,
        checked_at: None,
        error_message: None,
    };
    if !out.enabled {
        return out;
    }
    let mut cached = cache().lock().await;
    let stale = cached.as_ref().is_none_or(|c| {
        let ttl = if c.result.is_ok() {
            FRESH_FOR
        } else {
            RETRY_AFTER
        };
        c.at.elapsed() >= ttl
    });
    if stale {
        let checked_at = Utc::now();
        let result = fetch_latest(current).await;
        if let Err(e) = &result {
            tracing::info!(error = %e, "release check failed");
        }
        *cached = Some(Cached {
            at: Instant::now(),
            checked_at,
            result,
        });
    }
    let c = cached.as_ref().expect("filled above");
    out.checked_at = Some(c.checked_at);
    match &c.result {
        Ok(latest) => {
            out.update_available = is_newer(latest, current);
            out.latest_version_url = Some(release_url(latest));
            out.latest_version = Some(latest.clone());
        }
        Err(e) => out.error_message = Some(e.clone()),
    }
    out
}

/// Asks GitHub for the newest release; returns its version without the `v`.
async fn fetch_latest(current: &str) -> Result<String, String> {
    let user_agent = format!("sourcegull/{current}");
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(&user_agent)
        .build()
        .map_err(|e| e.to_string())?;
    let started_at = Utc::now();
    let start = Instant::now();
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;
    let status = response.as_ref().map_or(0, |r| r.status().as_u16());
    if outbound::enabled() {
        outbound::record(OutboundRequestEntry {
            started_at,
            method: "GET".to_owned(),
            url: LATEST_RELEASE_API.to_owned(),
            request_headers: vec![
                (
                    "accept".to_owned(),
                    vec!["application/vnd.github+json".to_owned()],
                ),
                ("user-agent".to_owned(), vec![user_agent.clone()]),
            ],
            status_code: status,
            response_headers: response.as_ref().map_or_else(
                |_| Vec::new(),
                |r| {
                    outbound::redact_headers(
                        r.headers().iter().map(|(k, v)| (k.as_str(), v.as_bytes())),
                    )
                },
            ),
            duration: start.elapsed(),
            error: response
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default(),
            client: "release check https://api.github.com/".to_owned(),
            caller: "site.updateCheck".to_owned(),
            ..Default::default()
        });
    }
    let response = response.map_err(|e| format!("GitHub could not be reached: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub answered {}", response.status()));
    }
    let release: Release = response
        .json()
        .await
        .map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let version = release.tag_name.trim_start_matches('v').to_owned();
    if parse_version(&version).is_none() {
        return Err(format!("unexpected release tag {:?}", release.tag_name));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(is_newer("0.4.0", "0.3.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("garbage", "0.3.0"));
    }

    #[test]
    fn release_urls() {
        assert_eq!(
            release_url("0.3.0"),
            "https://github.com/sourcegull/sourcegull/releases/tag/v0.3.0"
        );
        assert_eq!(release_url("v0.3.0"), release_url("0.3.0"));
    }
}
