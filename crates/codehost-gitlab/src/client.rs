//! The GitLab REST API client.
//!
//! Built on the shared [`RestClient`](sourcegull_codehost::rest::RestClient), which also keeps
//! credentials on the instance origin across redirects.
//!
//! - `reqwest` with rustls, HTTP/2, keep-alive pooling and gzip; a custom CA when the
//!   connection sets `certificate`.
//! - Token auth: `PRIVATE-TOKEN` for personal access tokens, `Authorization: Bearer` for OAuth.
//! - Every request first draws from the connection's [`RateLimiter`]. `RateLimit-Remaining:
//!   0` with `RateLimit-Reset` pauses the limiter until the reset; a 429 pauses it for
//!   `Retry-After` and retries; 502/503/504 and transport errors are retried with exponential
//!   backoff.
//! - Pagination always follows the `Link: <…>; rel="next"` header (keyset or offset; the caller
//!   picks the parameters of the first page). Next-page URLs must stay on the instance's
//!   origin, so the token is never sent elsewhere.
//! - The instance version (`GET /version`) is fetched once and cached to gate features.
//! - Writes (`POST`/`PUT` with a JSON body, for bulk changes) are retried only when
//!   GitLab cannot have applied them (429, failed connect).
//!
//! - [`GitLabClient::with_auth`] and [`GitLabClient::with_sudo`] derive clients for a user's
//!   OAuth token or an admin token impersonating a user (`Sudo` header), sharing the
//!   connection's HTTP pool and rate limiter.
//!
//! - An OAuth connection's client can carry a [`TokenSession`]: its access token is refreshed
//!   before it expires and after a 401, and the request is then retried once.
//!
//! - Every attempt is recorded in the outbound request log when one is installed
//!   ([`sourcegull_codehost::outbound`]), with the token and other secrets redacted.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
pub use sourcegull_codehost::ClientOptions;
pub use sourcegull_codehost::http::{next_link, retry_after};
use sourcegull_codehost::rest::{RestAuth, RestClient, RestConfig, RestPolicy, decode};
use sourcegull_codehost::{CodeHostError, RateLimiter, Result, TokenBucket};
use tokio::sync::OnceCell;
use url::Url;

use crate::config::{GitLabConnection, TokenType};
use crate::token::TokenSession;
use crate::types::{CiJob, GitLabVersion, Project, TokenInfo, VersionInfo};

/// Page size requested from list endpoints (GitLab's maximum).
pub const PER_PAGE: u32 = 100;

/// How the client authenticates.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// A personal, group or project access token (`PRIVATE-TOKEN` header).
    PrivateToken(String),
    /// An OAuth access token (`Authorization: Bearer`).
    Bearer(String),
    /// A CI job token (`JOB-TOKEN` header), valid while its job runs.
    JobToken(String),
    /// Anonymous (public projects only).
    None,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PrivateToken(_) => "PrivateToken(<redacted>)",
            Self::Bearer(_) => "Bearer(<redacted>)",
            Self::JobToken(_) => "JobToken(<redacted>)",
            Self::None => "None",
        })
    }
}

/// A GitLab API client for one instance and token. Cheap to share behind an `Arc`.
#[derive(Debug, Clone)]
pub struct GitLabClient {
    rest: RestClient,
    base: Url,
    api: Url,
    auth: Auth,
    /// `Sudo` header: the user (id or username) an admin token acts as.
    sudo: Option<String>,
    version: OnceCell<GitLabVersion>,
    /// The connection's refreshing OAuth tokens; replaces the token of [`Auth::Bearer`].
    session: Option<Arc<TokenSession>>,
}

/// GitLab's rate limit rules.
#[derive(Debug)]
struct GitLabPolicy;

impl RestPolicy for GitLabPolicy {
    fn name(&self) -> &'static str {
        "GitLab"
    }

    fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
        (status == StatusCode::TOO_MANY_REQUESTS)
            .then(|| retry_after(headers).unwrap_or(Duration::from_secs(1)))
    }

    fn budget_exhausted(&self, headers: &HeaderMap) -> Option<Duration> {
        sourcegull_codehost::rest::until_reset(headers, "ratelimit-remaining", "ratelimit-reset")
    }
}

/// The credentials a [`GitLabClient`] sends.
#[derive(Debug)]
struct GitLabAuth {
    auth: Auth,
    sudo: Option<String>,
    session: Option<Arc<TokenSession>>,
}

#[async_trait::async_trait]
impl RestAuth for GitLabAuth {
    async fn authorize(&self, mut req: RequestBuilder) -> Result<(RequestBuilder, Option<String>)> {
        let session_token = match &self.session {
            Some(s) => Some(s.access_token().await?),
            None => None,
        };
        match (&session_token, &self.auth) {
            (Some(t), _) | (None, Auth::Bearer(t)) => req = req.bearer_auth(t),
            (None, Auth::PrivateToken(t)) => req = req.header("PRIVATE-TOKEN", t),
            (None, Auth::JobToken(t)) => req = req.header("JOB-TOKEN", t),
            (None, Auth::None) => {}
        }
        if let Some(user) = &self.sudo {
            req = req.header("Sudo", user);
        }
        Ok((req, session_token))
    }

    async fn after_unauthorized(&self, rejected: &str) -> Result<bool> {
        // The token was revoked or expired early: retry once with a refreshed one.
        match &self.session {
            Some(session) => {
                session.after_unauthorized(rejected).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl GitLabClient {
    /// A client for `conn`, with a rate limiter of its own from its `rateLimit` (shared by
    /// nothing else; production code builds clients through the registry in `sourcegull-repos`,
    /// which uses [`with_limiter`](Self::with_limiter)).
    pub fn new(conn: &GitLabConnection, opts: ClientOptions) -> Result<Self> {
        let limiter: Arc<dyn RateLimiter> = match conn.requests_per_hour() {
            Some(rph) => Arc::new(TokenBucket::per_hour(rph)),
            None => Arc::new(TokenBucket::unlimited()),
        };
        Self::with_limiter(conn, limiter, opts)
    }

    /// A client for `conn` drawing from `limiter`: the connection's shared request budget.
    pub fn with_limiter(
        conn: &GitLabConnection,
        limiter: Arc<dyn RateLimiter>,
        opts: ClientOptions,
    ) -> Result<Self> {
        let auth = match conn.token_type {
            TokenType::Pat => Auth::PrivateToken(conn.token.clone()),
            TokenType::Oauth => Auth::Bearer(conn.token.clone()),
        };
        Self::with_parts(&conn.url, auth, conn.certificate.as_deref(), limiter, opts)
    }

    /// A client from its parts.
    pub fn with_parts(
        url: &str,
        auth: Auth,
        certificate: Option<&str>,
        limiter: Arc<dyn RateLimiter>,
        opts: ClientOptions,
    ) -> Result<Self> {
        let base = crate::config::normalize_base_url(url).map_err(CodeHostError::Config)?;
        let api = base
            .join("api/v4/")
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        let rest = RestClient::new(RestConfig {
            origin: base.clone(),
            log_client: format!("gitlab {base}"),
            certificate,
            default_headers: HeaderMap::new(),
            limiter,
            opts,
            policy: Arc::new(GitLabPolicy),
            auth: Arc::new(GitLabAuth {
                auth: auth.clone(),
                sudo: None,
                session: None,
            }),
        })?;
        Ok(Self {
            rest,
            base,
            api,
            auth,
            sudo: None,
            version: OnceCell::new(),
            session: None,
        })
    }

    /// This client with its REST client sending its current credentials.
    fn reauthorized(mut self) -> Self {
        self.rest = self.rest.with_auth(Arc::new(GitLabAuth {
            auth: self.auth.clone(),
            sudo: self.sudo.clone(),
            session: self.session.clone(),
        }));
        self
    }

    /// This client authenticating with the access token of `session`, refreshed before it
    /// expires and after a 401.
    #[must_use]
    pub fn with_token_session(mut self, session: Arc<TokenSession>) -> Self {
        self.auth = Auth::Bearer(session.current().access_token);
        self.session = Some(session);
        self.reauthorized()
    }

    /// The client's OAuth token session, if it has one.
    pub fn token_session(&self) -> Option<&Arc<TokenSession>> {
        self.session.as_ref()
    }

    /// A client for the same instance authenticating with `auth` instead, sharing this
    /// client's connection pool and rate limiter.
    pub fn with_auth(&self, auth: Auth) -> Self {
        Self {
            auth,
            sudo: None,
            session: None,
            ..self.clone()
        }
        .reauthorized()
    }

    /// A client sending `Sudo: <user>` (a GitLab user id or username), so an admin token with
    /// the `sudo` scope acts as that user.
    pub fn with_sudo(&self, user: &str) -> Self {
        Self {
            sudo: Some(user.to_owned()),
            ..self.clone()
        }
        .reauthorized()
    }

    /// The instance's normalized base URL, e.g. `https://gitlab.com/`.
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The absolute URL of an API path such as `projects?membership=true`.
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
    /// (a write may have been applied); every method is retried after a 429 or a failed
    /// connect.
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

    /// `POST` a JSON body to an API path and decode the JSON response.
    pub async fn post<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .send(Method::POST, self.api_url(path_and_query)?, Some(body))
            .await?;
        decode(resp).await
    }

    /// `PUT` a JSON body to an API path and decode the JSON response.
    pub async fn put<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .send(Method::PUT, self.api_url(path_and_query)?, Some(body))
            .await?;
        decode(resp).await
    }

    /// The client's authentication.
    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Fetches one page and the URL of the next (from the `Link` header).
    pub async fn get_page<T: DeserializeOwned>(&self, url: Url) -> Result<(Vec<T>, Option<Url>)> {
        self.rest.get_link_page(url).await
    }

    /// Streams every item of a paginated list, starting at `first` and following `Link`
    /// headers.
    pub fn paginate<T: DeserializeOwned + Send + 'static>(
        &self,
        first: Url,
    ) -> BoxStream<'_, Result<T>> {
        self.rest.paginate_link(first, |items: Vec<T>| items)
    }

    /// The instance version, fetched once and cached. `None` when it cannot be determined
    /// (e.g. the token may not read it); callers then assume a recent version.
    pub async fn version(&self) -> Option<GitLabVersion> {
        self.version
            .get_or_try_init(|| async {
                let info: VersionInfo = self.get("version").await?;
                GitLabVersion::parse(&info.version)
                    .ok_or_else(|| CodeHostError::Decode(format!("version {:?}", info.version)))
            })
            .await
            .map_err(|e| tracing::debug!(error = %e, "could not determine the GitLab version"))
            .ok()
            .copied()
    }

    /// One project by numeric id or full path.
    pub async fn project(&self, id_or_path: &str) -> Result<Project> {
        let encoded: String = url::form_urlencoded::byte_serialize(id_or_path.as_bytes()).collect();
        self.get(&format!("projects/{encoded}")).await
    }

    /// The job of the client's [`Auth::JobToken`] (`GET /job`), while it runs.
    pub async fn current_job(&self) -> Result<CiJob> {
        self.get("job").await
    }

    /// The scopes of the connection's personal access token (`GET
    /// /personal_access_tokens/self`). `None` for OAuth tokens and instances older than 15.5,
    /// which cannot tell.
    pub async fn token_scopes(&self) -> Result<Option<Vec<String>>> {
        if !matches!(self.auth, Auth::PrivateToken(_)) {
            return Ok(None);
        }
        if self
            .version()
            .await
            .is_some_and(|v| !v.supports_token_self())
        {
            return Ok(None);
        }
        match self.get::<TokenInfo>("personal_access_tokens/self").await {
            Ok(info) => Ok(Some(info.scopes)),
            // Group/project tokens and old instances answer 404.
            Err(CodeHostError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_urls_and_origin() {
        let c = GitLabClient::with_parts(
            "https://git.example.com/gitlab?x=1",
            Auth::None,
            None,
            Arc::new(TokenBucket::unlimited()),
            ClientOptions::default(),
        )
        .unwrap();
        assert_eq!(c.base_url().as_str(), "https://git.example.com/gitlab/");
        assert_eq!(
            c.api_url("/projects?membership=true").unwrap().as_str(),
            "https://git.example.com/gitlab/api/v4/projects?membership=true"
        );
        assert!(
            c.rest
                .same_origin(&Url::parse("https://git.example.com:443/x").unwrap())
        );
        assert!(
            !c.rest
                .same_origin(&Url::parse("https://evil.example.com/x").unwrap())
        );
        assert!(
            !c.rest
                .same_origin(&Url::parse("http://git.example.com/x").unwrap())
        );
    }
}
