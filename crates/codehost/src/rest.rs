//! The REST client the code host API clients are built on.
//!
//! - `reqwest` with rustls, HTTP/2, keep-alive pooling and gzip; a custom CA when the
//!   connection sets `certificate`.
//! - Credentials only go to the code host's own origin: [`RestClient::send`] refuses URLs on
//!   another origin (including `next` links), and redirects to another origin are not
//!   followed (the 3xx response is returned as an error).
//! - Every request first draws from the connection's [`RateLimiter`]. The host's
//!   [`RestPolicy`] says when a response reports an exhausted budget (the limiter pauses
//!   until the reset) and when it is a rate limit to wait out and retry. The policy's retried
//!   statuses (502/503/504 by default) and transport errors are retried with exponential
//!   backoff.
//! - Only `GET` is retried after 5xx and transport errors (a write may have been applied);
//!   every method is retried after a rate limit or a failed connect.
//! - The host's [`RestAuth`] adds credentials to each attempt and may refresh them after a
//!   401, in which case the request is retried once.
//! - Pagination follows `Link: <…>; rel="next"` headers ([`RestClient::paginate_link`]) or
//!   counts page numbers with a total-count header ([`RestClient::paginate_numbered`]).
//! - Every attempt is recorded in the outbound request log when one is installed
//!   ([`crate::outbound`]), with the token and other secrets redacted.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt, TryStreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use url::Url;

use crate::http::{header_str, next_link};
use crate::{ClientOptions, CodeHostError, RateLimiter, Result, outbound};

/// How a code host reports rate limits, which statuses it retries and how its errors map.
pub trait RestPolicy: Send + Sync + std::fmt::Debug {
    /// The host's name in log messages, e.g. `GitHub`.
    fn name(&self) -> &'static str;

    /// How long to wait before retrying a rate-limited response, or `None` when the response
    /// is not a rate limit.
    fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration>;

    /// The time until the budget resets when a response reports it exhausted.
    fn budget_exhausted(&self, _headers: &HeaderMap) -> Option<Duration> {
        None
    }

    /// Whether a `GET` answered with `status` is retried with backoff.
    fn retry_status(&self, status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
        )
    }

    /// The error for an unsuccessful response; `body` is the start of its body (or the
    /// status's reason when the body is empty).
    fn error(&self, status: StatusCode, url: &Url, body: String) -> CodeHostError {
        status_error(status, url, body)
    }
}

/// The default mapping of an unsuccessful status to an error.
pub fn status_error(status: StatusCode, url: &Url, body: String) -> CodeHostError {
    match status {
        StatusCode::UNAUTHORIZED => CodeHostError::Unauthorized,
        StatusCode::FORBIDDEN => CodeHostError::Forbidden(body),
        StatusCode::NOT_FOUND => CodeHostError::NotFound(url.path().to_owned()),
        _ => CodeHostError::Http {
            status: status.as_u16(),
            message: body,
        },
    }
}

/// Adds a code host's credentials to requests.
#[async_trait::async_trait]
pub trait RestAuth: Send + Sync + std::fmt::Debug {
    /// Adds the credentials to one attempt. Returns the request and, when the credentials can
    /// be refreshed, the token sent (passed to [`after_unauthorized`](Self::after_unauthorized)).
    async fn authorize(&self, req: RequestBuilder) -> Result<(RequestBuilder, Option<String>)>;

    /// Called once after a 401 to the request that sent `rejected`; `true` retries the
    /// request with refreshed credentials.
    async fn after_unauthorized(&self, _rejected: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Credentials sent as fixed headers (or none, for anonymous access).
#[derive(Clone, Default)]
pub struct HeaderAuth {
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl std::fmt::Debug for HeaderAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.headers.iter().map(|(k, _)| format!("{k}: <redacted>")))
            .finish()
    }
}

impl HeaderAuth {
    /// No credentials.
    pub fn none() -> Self {
        Self::default()
    }

    /// `Authorization: Bearer <token>`.
    pub fn bearer(token: &str) -> Result<Self> {
        Self::none().with("authorization", &format!("Bearer {token}"))
    }

    /// This auth also sending `name: value` (marked sensitive, so never logged).
    pub fn with(mut self, name: &str, value: &str) -> Result<Self> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| CodeHostError::Config(format!("header {name:?}: {e}")))?;
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| CodeHostError::Config(format!("invalid value for header {name}")))?;
        value.set_sensitive(true);
        self.headers.push((name, value));
        Ok(self)
    }
}

#[async_trait::async_trait]
impl RestAuth for HeaderAuth {
    async fn authorize(&self, mut req: RequestBuilder) -> Result<(RequestBuilder, Option<String>)> {
        for (k, v) in &self.headers {
            req = req.header(k.clone(), v.clone());
        }
        Ok((req, None))
    }
}

/// Whether `a` and `b` share scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Settings of a [`RestClient`].
pub struct RestConfig<'a> {
    /// The URL whose origin credentials may be sent to (the API or instance URL).
    pub origin: Url,
    /// The client's name in the outbound request log, e.g. `github https://github.com/`.
    pub log_client: String,
    /// A PEM CA certificate to trust in addition to the system roots.
    pub certificate: Option<&'a str>,
    /// Headers sent with every request (not credentials).
    pub default_headers: HeaderMap,
    /// Request budget.
    pub limiter: Arc<dyn RateLimiter>,
    /// Timeouts and retries.
    pub opts: ClientOptions,
    /// The host's rate limit, retry and error rules.
    pub policy: Arc<dyn RestPolicy>,
    /// The credentials.
    pub auth: Arc<dyn RestAuth>,
}

/// A REST client for one code host instance and set of credentials. Cheap to clone; clones
/// share the connection pool and the rate limiter.
#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    origin: Url,
    log_client: Arc<str>,
    limiter: Arc<dyn RateLimiter>,
    opts: ClientOptions,
    policy: Arc<dyn RestPolicy>,
    auth: Arc<dyn RestAuth>,
}

impl std::fmt::Debug for RestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestClient")
            .field("origin", &self.origin.as_str())
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RestClient {
    /// A client from `config`.
    pub fn new(config: RestConfig<'_>) -> Result<Self> {
        let origin = config.origin;
        let redirect_origin = origin.clone();
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("sourcegull/", env!("CARGO_PKG_VERSION")))
            .default_headers(config.default_headers)
            .timeout(config.opts.timeout)
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .gzip(true)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if !same_origin(attempt.url(), &redirect_origin) {
                    attempt.stop()
                } else if attempt.previous().len() >= 10 {
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }));
        if let Some(pem) = config.certificate {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|e| CodeHostError::Config(format!("certificate: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| CodeHostError::Config(format!("HTTP client: {e}")))?;
        Ok(Self {
            http,
            origin,
            log_client: config.log_client.into(),
            limiter: config.limiter,
            opts: config.opts,
            policy: config.policy,
            auth: config.auth,
        })
    }

    /// A client for the same instance with other credentials, sharing this client's
    /// connection pool and rate limiter.
    #[must_use]
    pub fn with_auth(&self, auth: Arc<dyn RestAuth>) -> Self {
        Self {
            auth,
            ..self.clone()
        }
    }

    /// The URL whose origin credentials are sent to.
    pub fn origin(&self) -> &Url {
        &self.origin
    }

    /// Whether `url` is on the client's origin.
    pub fn same_origin(&self, url: &Url) -> bool {
        same_origin(url, &self.origin)
    }

    /// Sends `GET url`; returns a successful response.
    pub async fn get_response(&self, url: Url) -> Result<Response> {
        self.send(Method::GET, url, None).await
    }

    /// `GET url` and decode the JSON body.
    pub async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        decode(self.get_response(url).await?).await
    }

    /// Sends a request with an optional JSON body, with auth, rate limiting and retries;
    /// returns a successful response.
    pub async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<&serde_json::Value>,
    ) -> Result<Response> {
        if !self.same_origin(&url) {
            return Err(CodeHostError::Decode(format!(
                "refusing to send credentials to {url} (not on {})",
                self.origin
            )));
        }
        let name = self.policy.name();
        let idempotent = method == Method::GET;
        let mut attempt = 0u32;
        let mut reauthorized = false;
        loop {
            self.limiter.acquire().await?;
            let mut req = self.http.request(method.clone(), url.clone());
            if let Some(body) = body {
                req = req.json(body);
            }
            let (req, sent_token) = self.auth.authorize(req).await?;
            let resp = match self.execute_logged(req, body).await {
                Ok(r) => r,
                Err(e) if attempt < self.opts.max_retries && (idempotent || e.is_connect()) => {
                    tracing::debug!(%url, error = %e, attempt, "{name} request failed; retrying");
                    self.backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(CodeHostError::Transport(e.without_url().to_string())),
            };
            self.observe_rate_limit(resp.headers()).await;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            if let Some(wait) = self.policy.rate_limit_wait(status, resp.headers()) {
                if attempt >= self.opts.max_retries || wait > self.opts.max_retry_wait {
                    return Err(CodeHostError::RateLimited {
                        retry_after: Some(wait),
                    });
                }
                tracing::info!(%url, ?wait, "{name} rate limit hit; pausing");
                self.limiter
                    .pause_until(tokio::time::Instant::now() + wait)
                    .await;
                attempt += 1;
                continue;
            }
            if idempotent && self.policy.retry_status(status) && attempt < self.opts.max_retries {
                self.backoff(attempt).await;
                attempt += 1;
                continue;
            }
            if status == StatusCode::UNAUTHORIZED
                && !reauthorized
                && let Some(rejected) = &sent_token
                && self.auth.after_unauthorized(rejected).await?
            {
                reauthorized = true;
                continue;
            }
            let body = body_snippet(resp).await;
            return Err(self.policy.error(status, &url, body));
        }
    }

    /// Sends `req` and records it in the outbound request log when one is installed.
    async fn execute_logged(
        &self,
        req: RequestBuilder,
        body: Option<&serde_json::Value>,
    ) -> reqwest::Result<Response> {
        if !outbound::enabled() {
            return req.send().await;
        }
        let req = req.build()?;
        let mut entry = outbound::OutboundRequestEntry {
            started_at: chrono::Utc::now(),
            method: req.method().to_string(),
            url: outbound::redact_url(req.url()),
            request_headers: outbound::redact_headers(
                req.headers()
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_bytes())),
            ),
            request_body: body
                .map(|b| outbound::truncate_body(&b.to_string()))
                .unwrap_or_default(),
            client: self.log_client.to_string(),
            caller: tracing::Span::current()
                .metadata()
                .map(|m| format!("{}::{}", m.target(), m.name()))
                .unwrap_or_default(),
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let res = self.http.execute(req).await;
        entry.duration = start.elapsed();
        // The error's URL may hold credentials: log it without.
        let res = res.map_err(reqwest::Error::without_url);
        match &res {
            Ok(r) => {
                entry.status_code = r.status().as_u16();
                entry.response_headers = outbound::redact_headers(
                    r.headers().iter().map(|(k, v)| (k.as_str(), v.as_bytes())),
                );
            }
            Err(e) => entry.error = e.to_string(),
        }
        outbound::record(entry);
        res
    }

    async fn backoff(&self, attempt: u32) {
        let delay = self.opts.retry_backoff * 2u32.saturating_pow(attempt.min(10));
        tokio::time::sleep(delay).await;
    }

    /// Pauses the limiter when the instance reports an exhausted budget.
    async fn observe_rate_limit(&self, headers: &HeaderMap) {
        let Some(wait) = self.policy.budget_exhausted(headers) else {
            return;
        };
        let wait = wait.min(self.opts.max_retry_wait);
        if !wait.is_zero() {
            tracing::info!(
                ?wait,
                "{} rate limit budget exhausted; pausing until reset",
                self.policy.name()
            );
            self.limiter
                .pause_until(tokio::time::Instant::now() + wait)
                .await;
        }
    }

    /// Fetches one page and the URL of the next (from the `Link` header).
    pub async fn get_link_page<P: DeserializeOwned>(&self, url: Url) -> Result<(P, Option<Url>)> {
        let resp = self.get_response(url).await?;
        let next = next_link(resp.headers());
        let page = decode(resp).await?;
        Ok((page, next))
    }

    /// Streams every item of a paginated list, starting at `first` and following `Link`
    /// headers until a page has no `next` link; `items` takes the items of each page.
    pub fn paginate_link<P, T, F>(&self, first: Url, items: F) -> BoxStream<'_, Result<T>>
    where
        P: DeserializeOwned + Send + 'static,
        T: Send + 'static,
        F: Fn(P) -> Vec<T> + Send + Sync + Copy + 'static,
    {
        stream::try_unfold(Some(first), move |next| async move {
            let Some(url) = next else {
                return Ok(None);
            };
            let (page, next) = self.get_link_page::<P>(url).await?;
            Ok(Some((stream::iter(items(page).into_iter().map(Ok)), next)))
        })
        .try_flatten()
        .boxed()
    }

    /// Fetches page `page` of the list at `url` with `page_param`/`size_param` set on it
    /// (replacing any already there), and the list's total item count from the
    /// `total_header` response header, when present.
    pub async fn get_numbered_page<P: DeserializeOwned>(
        &self,
        url: &Url,
        pages: &NumberedPages,
        page: u32,
    ) -> Result<(P, Option<u64>)> {
        let mut url = url.clone();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != pages.page_param && k != pages.size_param)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(kept)
            .append_pair(pages.page_param, &page.to_string())
            .append_pair(pages.size_param, &pages.per_page.to_string());
        let resp = self.get_response(url).await?;
        let total = header_str(resp.headers(), pages.total_header).and_then(|v| v.parse().ok());
        Ok((decode(resp).await?, total))
    }

    /// Streams every item of a list paginated by page number, counting up from 1 until a
    /// page is empty or the reported total was read.
    pub fn paginate_numbered<P, T, F>(
        &self,
        url: Url,
        pages: NumberedPages,
        items: F,
    ) -> BoxStream<'_, Result<T>>
    where
        P: DeserializeOwned + Send + 'static,
        T: Send + 'static,
        F: Fn(P) -> Vec<T> + Send + Sync + Copy + 'static,
    {
        let url = Arc::new(url);
        // (next page, items read so far); `None` once the list is exhausted.
        stream::try_unfold(Some((1u32, 0u64)), move |state| {
            let url = url.clone();
            async move {
                let Some((page, read)) = state else {
                    return Ok(None);
                };
                let (body, total) = self.get_numbered_page::<P>(&url, &pages, page).await?;
                let batch = items(body);
                if batch.is_empty() {
                    return Ok(None);
                }
                let read = read + batch.len() as u64;
                let next = match total {
                    Some(total) if read >= total => None,
                    _ => Some((page + 1, read)),
                };
                Ok(Some((stream::iter(batch.into_iter().map(Ok)), next)))
            }
        })
        .try_flatten()
        .boxed()
    }
}

/// Page-number pagination parameters.
#[derive(Debug, Clone, Copy)]
pub struct NumberedPages {
    /// The query parameter of the page number, e.g. `page`.
    pub page_param: &'static str,
    /// The query parameter of the page size, e.g. `limit`.
    pub size_param: &'static str,
    /// The page size requested.
    pub per_page: u32,
    /// The response header with the list's total item count, e.g. `x-total-count`.
    pub total_header: &'static str,
}

/// Decodes a JSON response body.
pub async fn decode<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| CodeHostError::Transport(e.without_url().to_string()))?;
    serde_json::from_slice(&bytes).map_err(|e| CodeHostError::Decode(e.to_string()))
}

/// The first 500 characters of the body, or the status's reason when it is empty.
async fn body_snippet(resp: Response) -> String {
    let reason = resp.status().canonical_reason().unwrap_or("").to_owned();
    match resp.text().await {
        Ok(t) if !t.trim().is_empty() => t.chars().take(500).collect(),
        _ => reason,
    }
}

/// The time until the reset timestamp in `reset_header` (Unix seconds) when
/// `remaining_header` is 0.
pub fn until_reset(
    headers: &HeaderMap,
    remaining_header: &str,
    reset_header: &str,
) -> Option<Duration> {
    let remaining = header_str(headers, remaining_header).and_then(|v| v.parse::<i64>().ok());
    if remaining != Some(0) {
        return None;
    }
    let reset = header_str(headers, reset_header).and_then(|v| v.parse::<i64>().ok())?;
    let secs = u64::try_from(reset - chrono::Utc::now().timestamp()).unwrap_or(0);
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenBucket;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Debug)]
    struct TestPolicy;

    impl RestPolicy for TestPolicy {
        fn name(&self) -> &'static str {
            "Test"
        }
        fn rate_limit_wait(&self, status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
            (status == StatusCode::TOO_MANY_REQUESTS)
                .then(|| crate::http::retry_after(headers).unwrap_or(Duration::from_secs(60)))
        }
    }

    fn opts() -> ClientOptions {
        ClientOptions {
            retry_backoff: Duration::from_millis(1),
            max_retry_wait: Duration::from_secs(5),
            ..ClientOptions::default()
        }
    }

    fn client_with(server: &MockServer, auth: Arc<dyn RestAuth>) -> RestClient {
        let origin = Url::parse(&server.uri()).unwrap();
        RestClient::new(RestConfig {
            log_client: format!("test {origin}"),
            origin,
            certificate: None,
            default_headers: HeaderMap::new(),
            limiter: Arc::new(TokenBucket::unlimited()),
            opts: opts(),
            policy: Arc::new(TestPolicy),
            auth,
        })
        .unwrap()
    }

    fn client(server: &MockServer) -> RestClient {
        client_with(
            server,
            Arc::new(HeaderAuth::none().with("private-token", "secret").unwrap()),
        )
    }

    fn url(server: &MockServer, p: &str) -> Url {
        Url::parse(&format!("{}{p}", server.uri())).unwrap()
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let server = MockServer::start().await;
        Mock::given(path("/limited"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/limited"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[1]"))
            .mount(&server)
            .await;
        Mock::given(path("/slow-down"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "600"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/write"))
            .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server);

        let v: Vec<u32> = c.get_json(url(&server, "/limited")).await.unwrap();
        assert!(v.is_empty());
        let v: Vec<u32> = c.get_json(url(&server, "/flaky")).await.unwrap();
        assert_eq!(v, [1]);
        assert_eq!(
            c.get_response(url(&server, "/slow-down"))
                .await
                .unwrap_err(),
            CodeHostError::RateLimited {
                retry_after: Some(Duration::from_secs(600))
            }
        );
        // A write is not retried after a 5xx.
        let err = c
            .send(
                Method::POST,
                url(&server, "/write"),
                Some(&serde_json::json!({})),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            CodeHostError::Http {
                status: 503,
                message: "busy".into()
            }
        );
        assert_eq!(
            c.get_response(url(&server, "/missing")).await.unwrap_err(),
            CodeHostError::NotFound("/missing".into())
        );
    }

    #[tokio::test]
    async fn credentials_stay_on_the_origin() {
        let server = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(path("/items"))
            .and(header("private-token", "secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("[1, 2]")
                    .insert_header(
                        "link",
                        format!("<{}/items?page=2>; rel=\"next\"", other.uri()),
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(path("/moved"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{}/x", other.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::any())
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(0)
            .mount(&other)
            .await;
        let c = client(&server);

        let got: Vec<Result<u32>> = c
            .paginate_link(url(&server, "/items"), |v: Vec<u32>| v)
            .collect()
            .await;
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[..2], [Ok(1), Ok(2)]);
        assert!(
            matches!(&got[2], Err(CodeHostError::Decode(m)) if m.starts_with("refusing to send credentials")),
            "{got:?}"
        );

        let err = c.get_response(url(&server, "/moved")).await.unwrap_err();
        assert!(
            matches!(err, CodeHostError::Http { status: 302, .. }),
            "{err:?}"
        );
        assert!(c.get_response(url(&other, "/x")).await.is_err());
    }

    #[tokio::test]
    async fn same_origin_redirects_are_followed() {
        let server = MockServer::start().await;
        Mock::given(path("/old"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
            .mount(&server)
            .await;
        Mock::given(path("/new"))
            .and(header("private-token", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[7]"))
            .mount(&server)
            .await;
        let v: Vec<u32> = client(&server)
            .get_json(url(&server, "/old"))
            .await
            .unwrap();
        assert_eq!(v, [7]);
    }

    #[tokio::test]
    async fn pagination_terminates() {
        let server = MockServer::start().await;
        Mock::given(path("/link"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[3]"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/link"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("[1, 2]")
                    .insert_header(
                        "link",
                        format!("<{}/link?page=2>; rel=\"next\"", server.uri()),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        // Page numbers: the total ends the first list, an empty page the second.
        for (p, body) in [("1", "[1, 2]"), ("2", "[3]")] {
            Mock::given(path("/total"))
                .and(query_param("page", p))
                .and(query_param("limit", "2"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(body)
                        .insert_header("x-total-count", "3"),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(path("/untotalled"))
                .and(query_param("page", p))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(path("/untotalled"))
            .and(query_param("page", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&server)
            .await;
        let c = client(&server);
        let pages = NumberedPages {
            page_param: "page",
            size_param: "limit",
            per_page: 2,
            total_header: "x-total-count",
        };

        let got: Vec<u32> = c
            .paginate_link(url(&server, "/link"), |v: Vec<u32>| v)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(got, [1, 2, 3]);
        let got: Vec<u32> = c
            .paginate_numbered(url(&server, "/total?limit=9"), pages, |v: Vec<u32>| v)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(got, [1, 2, 3]);
        let got: Vec<u32> = c
            .paginate_numbered(url(&server, "/untotalled"), pages, |v: Vec<u32>| v)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(got, [1, 2, 3]);
    }

    #[derive(Debug, Default)]
    struct RefreshingAuth {
        refreshes: AtomicU32,
    }

    #[async_trait::async_trait]
    impl RestAuth for RefreshingAuth {
        async fn authorize(&self, req: RequestBuilder) -> Result<(RequestBuilder, Option<String>)> {
            let token = format!("t{}", self.refreshes.load(Ordering::SeqCst));
            Ok((req.bearer_auth(&token), Some(token)))
        }
        async fn after_unauthorized(&self, rejected: &str) -> Result<bool> {
            assert_eq!(rejected, "t0", "refreshed more than once");
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    #[tokio::test]
    async fn refreshes_credentials_once_after_401() {
        let server = MockServer::start().await;
        Mock::given(path("/me"))
            .and(header("authorization", "Bearer t1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[1]"))
            .mount(&server)
            .await;
        Mock::given(path("/me"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let auth = Arc::new(RefreshingAuth::default());
        let c = client_with(&server, auth.clone());
        let v: Vec<u32> = c.get_json(url(&server, "/me")).await.unwrap();
        assert_eq!(v, [1]);
        assert_eq!(auth.refreshes.load(Ordering::SeqCst), 1);
    }
}
