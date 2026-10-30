//! The GitHub REST API client (github.com and GitHub Enterprise Server).
//!
//! Built on the shared [`RestClient`](sourcegull_codehost::rest::RestClient), which also keeps
//! credentials on the instance origin across redirects.
//!
//! - `reqwest` with rustls, HTTP/2, keep-alive pooling and gzip; a custom CA when the
//!   connection sets `certificate`.
//! - The token is sent as `Authorization: Bearer`, with `Accept: application/vnd.github+json`
//!   and a pinned `X-GitHub-Api-Version`. A connection with `githubApp` sends its current
//!   installation token ([`crate::app`]).
//! - Every request first draws from the connection's [`RateLimiter`]. `X-RateLimit-Remaining:
//!   0` with `X-RateLimit-Reset` pauses the limiter until the reset (primary rate limit). A
//!   429, or a 403 that is a rate limit (`Retry-After`, or no remaining budget), pauses it and
//!   retries (secondary rate limit). 502/503/504 and transport errors are retried with
//!   exponential backoff.
//! - Pagination follows the `Link: <…>; rel="next"` header. Next-page URLs must stay on the
//!   API's origin, so the token is never sent elsewhere.
//! - Every attempt is recorded in the outbound request log when one is installed
//!   ([`sourcegull_codehost::outbound`]), with the token and other secrets redacted.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use sourcegull_codehost::http::{header_str, retry_after};
use sourcegull_codehost::rest::{HeaderAuth, RestAuth, RestClient, RestConfig, RestPolicy, decode};
use sourcegull_codehost::{ClientOptions, CodeHostError, RateLimiter, Result, TokenBucket};
use url::Url;

use crate::app::{AppAuth, AppTokens};
use crate::config::GitHubConnection;
use crate::types::{Account, Repository, User};

/// Page size requested from list endpoints (GitHub's maximum).
pub const PER_PAGE: u32 = 100;

/// The REST API version requested.
pub const API_VERSION: &str = "2022-11-28";

/// A GitHub API client for one instance and token. Cheap to share behind an `Arc`.
#[derive(Clone)]
pub struct GitHubClient {
    rest: RestClient,
    base: Url,
    api: Url,
    /// The installation tokens of a connection with `githubApp`.
    app: Option<Arc<AppAuth>>,
}

impl std::fmt::Debug for GitHubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubClient")
            .field("api", &self.api.as_str())
            .finish_non_exhaustive()
    }
}

/// GitHub's rate limit rules.
#[derive(Debug)]
struct GitHubPolicy;

impl RestPolicy for GitHubPolicy {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
        rate_limit_wait(status, headers)
    }

    fn budget_exhausted(&self, headers: &HeaderMap) -> Option<Duration> {
        until_reset(headers)
    }
}

impl GitHubClient {
    /// A client for `conn`, with a rate limiter of its own from its `rateLimit` (shared by
    /// nothing else; production code builds clients through the registry in `sourcegull-repos`,
    /// which uses [`with_limiter`](Self::with_limiter)).
    pub fn new(conn: &GitHubConnection, opts: ClientOptions) -> Result<Self> {
        let limiter: Arc<dyn RateLimiter> = match conn.requests_per_hour() {
            Some(rph) => Arc::new(TokenBucket::per_hour(rph)),
            None => Arc::new(TokenBucket::unlimited()),
        };
        Self::with_limiter(conn, limiter, opts)
    }

    /// A client for `conn` drawing from `limiter`: the connection's shared request budget.
    pub fn with_limiter(
        conn: &GitHubConnection,
        limiter: Arc<dyn RateLimiter>,
        opts: ClientOptions,
    ) -> Result<Self> {
        let client = Self::with_parts(
            &conn.url,
            conn.token.clone(),
            conn.certificate.as_deref(),
            limiter,
            opts,
        )?;
        match &conn.github_app {
            Some(app) if conn.token.is_none() => {
                let tokens = AppTokens::shared(&client.api, app)?;
                Ok(client.with_app_tokens(tokens))
            }
            _ => Ok(client),
        }
    }

    /// This client authenticating as a GitHub App installation with the tokens of `tokens`.
    pub fn with_app_tokens(&self, tokens: Arc<AppTokens>) -> Self {
        let minting = self.rest.with_auth(Arc::new(HeaderAuth::none()));
        let app = Arc::new(AppAuth::new(tokens, minting));
        Self {
            rest: self.rest.with_auth(app.clone()),
            app: Some(app),
            ..self.clone()
        }
    }

    /// Whether the client authenticates as a GitHub App installation.
    pub fn is_app(&self) -> bool {
        self.app.is_some()
    }

    /// The current installation token of a GitHub App connection (minted when needed);
    /// `None` for token connections.
    pub async fn installation_token(&self) -> Result<Option<String>> {
        match &self.app {
            Some(app) => app.token().await.map(Some),
            None => Ok(None),
        }
    }

    /// The cached installation token, when it is current.
    pub fn cached_installation_token(&self) -> Option<String> {
        self.app.as_ref().and_then(|a| a.cached())
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
        let auth: Arc<dyn RestAuth> = Arc::new(match &token {
            Some(t) => HeaderAuth::bearer(t)?,
            None => HeaderAuth::none(),
        });
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            "accept",
            HeaderValue::from_static("application/vnd.github+json"),
        );
        default_headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        let rest = RestClient::new(RestConfig {
            origin: api.clone(),
            log_client: format!("github {base}"),
            certificate,
            default_headers,
            limiter,
            opts,
            policy: Arc::new(GitHubPolicy),
            auth,
        })?;
        Ok(Self {
            rest,
            base,
            api,
            app: None,
        })
    }

    /// A client for the same instance authenticating with `token` (e.g. a user's OAuth token),
    /// sharing this client's connection pool and rate limiter.
    pub fn with_token(&self, token: &str) -> Result<Self> {
        Ok(Self {
            rest: self.rest.with_auth(Arc::new(HeaderAuth::bearer(token)?)),
            app: None,
            ..self.clone()
        })
    }

    /// The instance's normalized base URL, e.g. `https://github.com/`.
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The absolute URL of an API path such as `user/repos?affiliation=owner`.
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

    /// `GET` an API path and return the decoded body with the response headers.
    pub async fn get_with_headers<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
    ) -> Result<(T, HeaderMap)> {
        let resp = self.get_response(self.api_url(path_and_query)?).await?;
        let headers = resp.headers().clone();
        Ok((decode(resp).await?, headers))
    }

    /// Fetches one page and the URL of the next (from the `Link` header).
    pub async fn get_page<P: DeserializeOwned>(&self, url: Url) -> Result<(P, Option<Url>)> {
        self.rest.get_link_page(url).await
    }

    /// Streams every item of a paginated list whose pages are JSON arrays, starting at
    /// `first` and following `Link` headers.
    pub fn paginate<T: DeserializeOwned + Send + 'static>(
        &self,
        first: Url,
    ) -> BoxStream<'_, Result<T>> {
        self.paginate_with(first, |items: Vec<T>| items)
    }

    /// Streams every item of a paginated list whose pages are objects of type `P` (e.g.
    /// search results), taking the items of each page with `items`.
    pub fn paginate_with<P, T, F>(&self, first: Url, items: F) -> BoxStream<'_, Result<T>>
    where
        P: DeserializeOwned + Send + 'static,
        T: Send + 'static,
        F: Fn(P) -> Vec<T> + Send + Sync + Copy + 'static,
    {
        self.rest.paginate_link(first, items)
    }

    /// One repository by `owner/name`.
    pub async fn repo(&self, name_with_owner: &str) -> Result<Repository> {
        let path: Vec<String> = name_with_owner
            .split('/')
            .map(|s| url::form_urlencoded::byte_serialize(s.as_bytes()).collect())
            .collect();
        self.get(&format!("repos/{}", path.join("/"))).await
    }

    /// A user or organization by login.
    pub async fn account(&self, login: &str) -> Result<Account> {
        let login: String = url::form_urlencoded::byte_serialize(login.as_bytes()).collect();
        self.get(&format!("users/{login}")).await
    }

    /// One repository by numeric id (stable across renames and transfers).
    pub async fn repo_by_id(&self, id: &str) -> Result<Repository> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(CodeHostError::NotFound(format!("repository id {id:?}")));
        }
        self.get(&format!("repositories/{id}")).await
    }

    /// The token's user and the scopes of a classic token (`X-OAuth-Scopes`; `None` for
    /// fine-grained tokens, which do not report scopes).
    pub async fn current_user(&self) -> Result<(User, Option<Vec<String>>)> {
        let (user, headers): (User, _) = self.get_with_headers("user").await?;
        let scopes = header_str(&headers, "x-oauth-scopes").map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        });
        Ok((user, scopes))
    }
}

/// How long to wait before retrying a rate-limited response, or `None` when `status` is not a
/// rate limit. GitHub answers 429 or 403 for both the primary limit (no remaining budget) and
/// secondary limits (`Retry-After`).
fn rate_limit_wait(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::FORBIDDEN {
        return None;
    }
    if let Some(wait) = retry_after(headers) {
        return Some(wait);
    }
    if let Some(wait) = until_reset(headers) {
        return Some(wait);
    }
    // A 429 without hints; GitHub asks to wait at least a minute for secondary limits.
    (status == StatusCode::TOO_MANY_REQUESTS).then_some(Duration::from_secs(60))
}

/// The time until `X-RateLimit-Reset` when `X-RateLimit-Remaining` is 0.
fn until_reset(headers: &HeaderMap) -> Option<Duration> {
    sourcegull_codehost::rest::until_reset(headers, "x-ratelimit-remaining", "x-ratelimit-reset")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn api_urls_and_origin() {
        let c = GitHubClient::with_parts(
            "https://github.com",
            None,
            None,
            Arc::new(TokenBucket::unlimited()),
            ClientOptions::default(),
        )
        .unwrap();
        assert_eq!(c.base_url().as_str(), "https://github.com/");
        assert_eq!(
            c.api_url("/user/repos?per_page=100").unwrap().as_str(),
            "https://api.github.com/user/repos?per_page=100"
        );
        assert!(
            c.rest
                .same_origin(&Url::parse("https://api.github.com/x").unwrap())
        );
        assert!(
            !c.rest
                .same_origin(&Url::parse("https://github.com/x").unwrap())
        );

        let c = GitHubClient::with_parts(
            "https://ghe.example.com",
            None,
            None,
            Arc::new(TokenBucket::unlimited()),
            ClientOptions::default(),
        )
        .unwrap();
        assert_eq!(
            c.api_url("orgs/o/repos").unwrap().as_str(),
            "https://ghe.example.com/api/v3/orgs/o/repos"
        );
        assert!(
            !c.rest
                .same_origin(&Url::parse("https://evil.example.com/api/v3/x").unwrap())
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
            rate_limit_wait(StatusCode::FORBIDDEN, &h),
            Some(Duration::from_secs(3))
        );
        assert_eq!(rate_limit_wait(StatusCode::NOT_FOUND, &h), None);

        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        let reset = (chrono::Utc::now().timestamp() + 30).to_string();
        h.insert("x-ratelimit-reset", HeaderValue::from_str(&reset).unwrap());
        let wait = rate_limit_wait(StatusCode::FORBIDDEN, &h).unwrap();
        assert!(wait <= Duration::from_secs(30) && wait >= Duration::from_secs(28));
    }
}
