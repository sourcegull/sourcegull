//! The Gitea REST API client.
//!
//! Built on the shared [`RestClient`](sourcegull_codehost::rest::RestClient), which also keeps
//! credentials on the instance origin across redirects.
//!
//! - `reqwest` with rustls, HTTP/2, keep-alive pooling and gzip; a custom CA when the
//!   connection sets `certificate`.
//! - The token is sent as `Authorization: token <token>`; [`GiteaClient::with_sudo`] adds
//!   `Sudo: <username>`, with which a site administrator's token acts as that user.
//! - Every request first draws from the connection's [`RateLimiter`]. Gitea itself does not
//!   rate limit its API, but a proxy in front of it may: a 429 pauses the limiter for its
//!   `Retry-After` (a minute without one) and retries. 502/503/504 and transport errors are
//!   retried with exponential backoff.
//! - Pagination counts `page` up from 1 with `limit` [`PER_PAGE`], until a page is empty or
//!   `X-Total-Count` items were read. Gitea's `Link` headers are built from its configured
//!   root URL, which may not be the URL Sourcegull reaches it at, so they are not followed.
//! - Every attempt is recorded in the outbound request log when one is installed
//!   ([`sourcegull_codehost::outbound`]), with the token and other secrets redacted.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use sourcegull_codehost::http::retry_after;
use sourcegull_codehost::rest::{
    HeaderAuth, NumberedPages, RestAuth, RestClient, RestConfig, RestPolicy,
};
use sourcegull_codehost::{ClientOptions, CodeHostError, RateLimiter, Result, TokenBucket};
use url::Url;

use crate::config::GiteaConnection;
use crate::types::{Repository, User, Version};

/// Page size requested from list endpoints (Gitea's default maximum, `MAX_RESPONSE_ITEMS`).
/// An instance configured with a lower maximum answers smaller pages, which pagination
/// handles.
pub const PER_PAGE: u32 = 50;

/// Gitea's list pagination: `page` from 1, `limit` items, `X-Total-Count` total.
const PAGES: NumberedPages = NumberedPages {
    page_param: "page",
    size_param: "limit",
    per_page: PER_PAGE,
    total_header: "x-total-count",
};

/// A Gitea API client for one instance and token. Cheap to share behind an `Arc`.
#[derive(Clone)]
pub struct GiteaClient {
    rest: RestClient,
    base: Url,
    api: Url,
    token: Option<String>,
}

impl std::fmt::Debug for GiteaClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiteaClient")
            .field("api", &self.api.as_str())
            .finish_non_exhaustive()
    }
}

/// Gitea's rate limit rules.
#[derive(Debug)]
struct GiteaPolicy;

impl RestPolicy for GiteaPolicy {
    fn name(&self) -> &'static str {
        "Gitea"
    }

    fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
        rate_limit_wait(status, headers)
    }
}

impl GiteaClient {
    /// A client for `conn`, with a rate limiter of its own from its `rateLimit` (shared by
    /// nothing else; production code builds clients through the registry in `sourcegull-repos`,
    /// which uses [`with_limiter`](Self::with_limiter)).
    pub fn new(conn: &GiteaConnection, opts: ClientOptions) -> Result<Self> {
        let limiter: Arc<dyn RateLimiter> = match conn.requests_per_hour() {
            Some(rph) => Arc::new(TokenBucket::per_hour(rph)),
            None => Arc::new(TokenBucket::unlimited()),
        };
        Self::with_limiter(conn, limiter, opts)
    }

    /// A client for `conn` drawing from `limiter`: the connection's shared request budget.
    pub fn with_limiter(
        conn: &GiteaConnection,
        limiter: Arc<dyn RateLimiter>,
        opts: ClientOptions,
    ) -> Result<Self> {
        Self::with_parts(
            &conn.url,
            Some(conn.token.clone()),
            conn.certificate.as_deref(),
            limiter,
            opts,
        )
    }

    /// A client from its parts; `token: None` is anonymous.
    pub fn with_parts(
        url: &str,
        token: Option<String>,
        certificate: Option<&str>,
        limiter: Arc<dyn RateLimiter>,
        opts: ClientOptions,
    ) -> Result<Self> {
        let base = crate::config::normalize_base_url(url).map_err(CodeHostError::Config)?;
        let api = crate::config::api_url(&base).map_err(CodeHostError::Config)?;
        let auth = token_auth(token.as_deref(), None)?;
        let mut default_headers = HeaderMap::new();
        default_headers.insert("accept", HeaderValue::from_static("application/json"));
        let rest = RestClient::new(RestConfig {
            origin: api.clone(),
            log_client: format!("gitea {base}"),
            certificate,
            default_headers,
            limiter,
            opts,
            policy: Arc::new(GiteaPolicy),
            auth,
        })?;
        Ok(Self {
            rest,
            base,
            api,
            token,
        })
    }

    /// A client for the same instance sending `token` instead, sharing this client's
    /// connection pool and rate limiter.
    pub fn with_token(&self, token: &str) -> Result<Self> {
        Ok(Self {
            rest: self.rest.with_auth(token_auth(Some(token), None)?),
            token: Some(token.to_owned()),
            ..self.clone()
        })
    }

    /// A client sending `Sudo: <username>` with this client's token, so a site
    /// administrator's token acts as that user.
    pub fn with_sudo(&self, username: &str) -> Result<Self> {
        Ok(Self {
            rest: self
                .rest
                .with_auth(token_auth(self.token.as_deref(), Some(username))?),
            ..self.clone()
        })
    }

    /// The instance's normalized base URL, e.g. `https://gitea.example.com/`.
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The absolute URL of an API path such as `user/repos`.
    pub fn api_url(&self, path_and_query: &str) -> Result<Url> {
        self.api
            .join(path_and_query.trim_start_matches('/'))
            .map_err(|e| CodeHostError::Config(format!("API path {path_and_query:?}: {e}")))
    }

    /// Sends `GET url` with auth, rate limiting and retries; returns a successful response.
    pub async fn get_response(&self, url: Url) -> Result<Response> {
        self.rest.get_response(url).await
    }

    /// Sends a request with an optional JSON body, with auth, rate limiting and retries;
    /// returns a successful response. Only `GET` is retried after 5xx and transport errors
    /// (a write may have been applied); every method is retried after a rate limit or a
    /// failed connect.
    pub async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<&serde_json::Value>,
    ) -> Result<Response> {
        self.rest.send(method, url, body).await
    }

    /// `GET` an API path and decode the JSON body.
    pub async fn get<T: DeserializeOwned>(&self, path_and_query: &str) -> Result<T> {
        self.rest.get_json(self.api_url(path_and_query)?).await
    }

    /// Fetches page `page` of the list at `url` (`page` and `limit` are set on it) and the
    /// list's total item count (`X-Total-Count`), when Gitea reports it.
    pub async fn get_page<P: DeserializeOwned>(
        &self,
        url: &Url,
        page: u32,
    ) -> Result<(P, Option<u64>)> {
        self.rest.get_numbered_page(url, &PAGES, page).await
    }

    /// Streams every item of a paginated list whose pages are JSON arrays.
    pub fn paginate<T: DeserializeOwned + Send + 'static>(
        &self,
        url: Url,
    ) -> BoxStream<'_, Result<T>> {
        self.paginate_with(url, |items: Vec<T>| items)
    }

    /// Streams every item of a paginated list whose pages are objects of type `P` (e.g.
    /// search results), taking the items of each page with `items`.
    pub fn paginate_with<P, T, F>(&self, url: Url, items: F) -> BoxStream<'_, Result<T>>
    where
        P: DeserializeOwned + Send + 'static,
        T: Send + 'static,
        F: Fn(P) -> Vec<T> + Send + Sync + Copy + 'static,
    {
        self.rest.paginate_numbered(url, PAGES, items)
    }

    /// One repository by `owner/name`.
    pub async fn repo(&self, name_with_owner: &str) -> Result<Repository> {
        let path: Vec<String> = name_with_owner
            .split('/')
            .map(|s| url::form_urlencoded::byte_serialize(s.as_bytes()).collect())
            .collect();
        self.get(&format!("repos/{}", path.join("/"))).await
    }

    /// One repository by numeric id (stable across renames and transfers).
    pub async fn repo_by_id(&self, id: &str) -> Result<Repository> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CodeHostError::NotFound(format!("repository id {id:?}")));
        }
        self.get(&format!("repositories/{id}")).await
    }

    /// The token's user.
    pub async fn current_user(&self) -> Result<User> {
        self.get("user").await
    }

    /// The instance's Gitea version, e.g. `1.24.6`.
    pub async fn version(&self) -> Result<String> {
        self.get::<Version>("version").await.map(|v| v.version)
    }
}

/// The headers authenticating as `token` (anonymous without one), acting as `sudo` when set.
fn token_auth(token: Option<&str>, sudo: Option<&str>) -> Result<Arc<dyn RestAuth>> {
    let mut auth = HeaderAuth::none();
    if let Some(t) = token {
        auth = auth.with("authorization", &format!("token {t}"))?;
    }
    if let Some(user) = sudo {
        auth = auth.with("sudo", user)?;
    }
    Ok(Arc::new(auth))
}

/// How long to wait before retrying a rate-limited response, or `None` when `status` is not a
/// rate limit.
fn rate_limit_wait(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    (status == StatusCode::TOO_MANY_REQUESTS)
        .then(|| retry_after(headers).unwrap_or(Duration::from_secs(60)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn api_urls_and_origin() {
        let c = GiteaClient::with_parts(
            "https://example.com/git",
            None,
            None,
            Arc::new(TokenBucket::unlimited()),
            ClientOptions::default(),
        )
        .unwrap();
        assert_eq!(c.base_url().as_str(), "https://example.com/git/");
        assert_eq!(
            c.api_url("/user/repos").unwrap().as_str(),
            "https://example.com/git/api/v1/user/repos"
        );
        assert!(
            c.rest
                .same_origin(&Url::parse("https://example.com/x").unwrap())
        );
        assert!(
            !c.rest
                .same_origin(&Url::parse("https://evil.example.com/git/api/v1/x").unwrap())
        );
    }

    #[test]
    fn rate_limit_responses() {
        let mut h = HeaderMap::new();
        assert_eq!(rate_limit_wait(StatusCode::FORBIDDEN, &h), None);
        assert_eq!(
            rate_limit_wait(StatusCode::TOO_MANY_REQUESTS, &h),
            Some(Duration::from_secs(60))
        );
        h.insert("retry-after", HeaderValue::from_static("3"));
        assert_eq!(
            rate_limit_wait(StatusCode::TOO_MANY_REQUESTS, &h),
            Some(Duration::from_secs(3))
        );
        assert_eq!(rate_limit_wait(StatusCode::FORBIDDEN, &h), None);
    }
}
