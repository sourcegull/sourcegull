//! Helpers shared by the code host REST clients: client tuning and response headers.

use std::time::Duration;

use http::HeaderMap;
use url::Url;

/// API client tuning.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Timeout of one request.
    pub timeout: Duration,
    /// Retries after a 429, a 502/503/504 or a transport error.
    pub max_retries: u32,
    /// The longest `Retry-After` the client waits for; longer requests fail with
    /// [`CodeHostError::RateLimited`](crate::CodeHostError::RateLimited).
    pub max_retry_wait: Duration,
    /// First backoff delay for 5xx/transport retries; doubles with each attempt.
    pub retry_backoff: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_retries: 3,
            max_retry_wait: Duration::from_secs(120),
            retry_backoff: Duration::from_millis(500),
        }
    }
}

/// A header's value as trimmed text, if present and valid.
pub fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// Parses `Retry-After` (seconds or an HTTP date).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = header_str(headers, "retry-after")?;
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let secs = (at.timestamp() - chrono::Utc::now().timestamp()).max(0);
    Some(Duration::from_secs(u64::try_from(secs).unwrap_or(0)))
}

/// The `rel="next"` URL of a `Link` header.
pub fn next_link(headers: &HeaderMap) -> Option<Url> {
    headers
        .get_all("link")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|part| {
            let mut pieces = part.split(';').map(str::trim);
            let target = pieces.next()?.strip_prefix('<')?.strip_suffix('>')?;
            pieces
                .any(|p| p.replace(' ', "") == "rel=\"next\"" || p == "rel=next")
                .then(|| Url::parse(target).ok())
                .flatten()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn parses_link_and_retry_after() {
        let mut h = HeaderMap::new();
        h.insert(
            "link",
            HeaderValue::from_static(
                "<https://gitlab.com/api/v4/projects?page=1>; rel=\"first\", \
                 <https://gitlab.com/api/v4/projects?id_after=5&per_page=100>; rel=\"next\"",
            ),
        );
        assert_eq!(
            next_link(&h).unwrap().as_str(),
            "https://gitlab.com/api/v4/projects?id_after=5&per_page=100"
        );
        assert!(next_link(&HeaderMap::new()).is_none());

        h.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(7)));
        h.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), Some(Duration::ZERO));
    }
}
