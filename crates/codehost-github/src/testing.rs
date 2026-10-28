//! A mock GitHub Enterprise Server API for tests (feature `testing`).
//!
//! Serves the endpoints discovery uses under `/api/v3` from an in-memory repository list:
//! `GET /user` (with `X-OAuth-Scopes` when [`MockState::scopes`] is set), `/user/repos`
//! (repositories in [`MockState::affiliated`]), `/orgs/:org/repos` (repositories whose owner is
//! the org), `/repos/:owner/:name`, `/repositories/:id`, `/repositories` (paged by `since`)
//! and `/search/repositories` (the query is matched as a substring of `full_name`, or `org:o`
//! as the owner). Offset pages are small (default 2 items) so `Link` pagination is exercised.
//! Users of [`MockState::users`] sign in with their own tokens: `GET /user` describes them (with
//! their scopes) and `/user/repos` lists their repositories, and
//! `/repos/:owner/:name/collaborators` lists the users with the repository (`403` when
//! [`MockState::collaborators_forbidden`]). As a GitHub App ([`MockGitHub::enable_app`]),
//! `POST /app/installations/:id/access_tokens` verifies the JWT against
//! [`APP_PRIVATE_KEY`]'s public key and issues installation tokens that expire after
//! [`MockState::installation_token_ttl_secs`]; they are accepted like the token, and
//! `GET /installation/repositories` lists the [`MockState::affiliated`] repositories to them.
//! For sign-in it also serves `/user/emails`, `/user/orgs` and the OAuth endpoints `GET
//! /login/oauth/authorize` and `POST /login/oauth/access_token` (which issues the accepted
//! token). Every request is recorded, and the next N requests can be made to fail with a rate
//! limit.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

/// The private key (PKCS#1 PEM) of the mock GitHub App.
pub const APP_PRIVATE_KEY: &str = include_str!("testdata/app-key.pem");
/// The public key the mock verifies App JWTs with.
const APP_PUBLIC_KEY: &str = include_str!("testdata/app-key.pub.pem");

/// A GitHub user with a token of their own and the repositories they can read.
#[derive(Debug, Clone)]
pub struct MockUser {
    pub id: i64,
    pub login: String,
    /// The user's OAuth token.
    pub token: String,
    /// `X-OAuth-Scopes` of the token; `None` leaves the header out.
    pub scopes: Option<Vec<String>>,
    /// Ids of the repositories the user can read (`/user/repos`, and collaborators).
    pub repos: HashSet<i64>,
}

impl MockUser {
    /// A user with token `token-<id>` and the `repo` scope.
    pub fn new(id: i64, login: &str) -> Self {
        Self {
            id,
            login: login.to_owned(),
            token: format!("token-{id}"),
            scopes: Some(vec!["read:user".into(), "repo".into()]),
            repos: HashSet::new(),
        }
    }

    /// The user can read repository `id`.
    #[must_use]
    pub fn reads(mut self, id: i64) -> Self {
        self.repos.insert(id);
        self
    }
}

/// The mutable state behind a [`MockGitHub`].
#[derive(Debug)]
pub struct MockState {
    /// Repositories as GitHub returns them.
    pub repos: Vec<Value>,
    /// Ids of the repositories `GET /user/repos` lists.
    pub affiliated: HashSet<i64>,
    /// The accepted token.
    pub token: String,
    /// Users with tokens of their own.
    pub users: Vec<MockUser>,
    /// Whether `GET /repos/:owner/:name/collaborators` answers `403` (no admin access).
    pub collaborators_forbidden: bool,
    /// The login of the token's user.
    pub login: String,
    /// The id of the token's user.
    pub user_id: i64,
    /// The user's display name.
    pub name: Option<String>,
    /// The user's profile email.
    pub email: Option<String>,
    /// `GET /user/emails`: `{email, primary, verified}` objects.
    pub emails: Vec<Value>,
    /// Organizations of the user (`GET /user/orgs`).
    pub orgs: Vec<String>,
    /// The OAuth application's client id and secret (`/login/oauth/*`).
    pub client_id: String,
    pub client_secret: String,
    /// Whether `GET /login/oauth/authorize` signs the user in; otherwise it redirects back with
    /// `error=access_denied`.
    pub authorize: bool,
    /// Authorization codes issued and not yet exchanged, with their PKCE challenge.
    pub codes: HashMap<String, String>,
    /// `X-OAuth-Scopes` of `GET /user`; `None` leaves the header out (fine-grained tokens).
    pub scopes: Option<Vec<String>>,
    /// Maximum page size served.
    pub page_size: usize,
    /// The next this-many API requests get `403` with `X-RateLimit-Remaining: 0` and a reset
    /// [`rate_limit_reset_secs`](Self::rate_limit_reset_secs) from now.
    pub rate_limited: u32,
    /// Seconds until the reset of rate-limited responses (default 0).
    pub rate_limit_reset_secs: i64,
    /// Every request's path and query, in order.
    pub requests: Vec<String>,
    /// The GitHub App's ID; `None` answers `404` to token requests.
    pub app_id: Option<u64>,
    /// The App's installation that tokens can be requested for.
    pub installation_id: u64,
    /// Lifetime of issued installation tokens.
    pub installation_token_ttl_secs: i64,
    /// How long a token request takes, to let concurrent requests overlap.
    pub mint_delay: std::time::Duration,
    /// Installation tokens issued, with their expiry (Unix seconds).
    pub installation_tokens: Vec<(String, i64)>,
    base: String,
}

impl MockState {
    /// Whether `token` is an unexpired installation token.
    fn installation_token(&self, token: Option<&str>) -> bool {
        let now = chrono::Utc::now().timestamp();
        token.is_some_and(|t| {
            self.installation_tokens
                .iter()
                .any(|(it, exp)| it == t && *exp > now)
        })
    }
}

/// A running mock server; stopped when dropped.
pub struct MockGitHub {
    /// Base URL, e.g. `http://127.0.0.1:41234` (the API is under `/api/v3`).
    pub url: String,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockGitHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A repository object with the fields discovery reads. `id` also determines
/// `stargazers_count`.
pub fn repo(id: i64, full_name: &str) -> Value {
    json!({
        "id": id,
        "node_id": format!("R_{id}"),
        "full_name": full_name,
        "description": format!("repo {id}"),
        "html_url": format!("https://github.example.com/{full_name}"),
        "clone_url": format!("https://github.example.com/{full_name}.git"),
        "ssh_url": format!("git@github.example.com:{full_name}.git"),
        "default_branch": "main",
        "private": true,
        "visibility": "private",
        "archived": false,
        "disabled": false,
        "fork": false,
        "stargazers_count": id,
        "forks_count": 0,
        "topics": [],
        "size": 10,
        "pushed_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "created_at": "2025-01-01T00:00:00Z",
    })
}

impl MockGitHub {
    /// Starts a server on a free local port accepting `token`.
    pub async fn start(token: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitHub");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(MockState {
            repos: Vec::new(),
            affiliated: HashSet::new(),
            token: token.to_owned(),
            users: Vec::new(),
            collaborators_forbidden: false,
            login: "octocat".into(),
            user_id: 1,
            name: None,
            email: None,
            emails: Vec::new(),
            orgs: Vec::new(),
            client_id: "mock-client".into(),
            client_secret: "mock-secret".into(),
            authorize: true,
            codes: HashMap::new(),
            scopes: Some(vec!["repo".into()]),
            page_size: 2,
            rate_limited: 0,
            rate_limit_reset_secs: 0,
            requests: Vec::new(),
            app_id: None,
            installation_id: 0,
            installation_token_ttl_secs: 3600,
            mint_delay: std::time::Duration::ZERO,
            installation_tokens: Vec::new(),
            base: url.clone(),
        }));
        let app = Router::new()
            .route("/api/v3/user", get(current_user))
            .route("/api/v3/user/emails", get(user_emails))
            .route("/api/v3/user/orgs", get(user_orgs))
            .route("/login/oauth/authorize", get(authorize))
            .route(
                "/login/oauth/access_token",
                axum::routing::post(access_token),
            )
            .route("/api/v3/user/repos", get(user_repos))
            .route("/api/v3/orgs/{org}/repos", get(org_repos))
            .route("/api/v3/repos/{owner}/{name}", get(get_repo))
            .route(
                "/api/v3/repos/{owner}/{name}/collaborators",
                get(collaborators),
            )
            .route("/api/v3/repositories", get(all_repos))
            .route("/api/v3/repositories/{id}", get(repo_by_id))
            .route("/api/v3/search/repositories", get(search_repos))
            .route(
                "/api/v3/app/installations/{id}/access_tokens",
                axum::routing::post(installation_access_token),
            )
            .route("/api/v3/installation/repositories", get(installation_repos))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, state, task }
    }

    /// Runs `f` on the state.
    pub fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Acts as GitHub App `app_id` installed as `installation_id` (keys [`APP_PRIVATE_KEY`]).
    pub fn enable_app(&self, app_id: u64, installation_id: u64) {
        self.with(|s| {
            s.app_id = Some(app_id);
            s.installation_id = installation_id;
        });
    }

    /// The number of installation tokens issued.
    pub fn installation_tokens_issued(&self) -> usize {
        self.with(|s| s.installation_tokens.len())
    }

    /// The recorded requests (path and query).
    pub fn requests(&self) -> Vec<String> {
        self.with(|s| s.requests.clone())
    }
}

type Shared = State<Arc<Mutex<MockState>>>;

/// Records the request and checks the token and the rate limit; `Some` is the refusal to send.
fn admit(s: &mut MockState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    s.requests.push(
        uri.path_and_query()
            .map_or_else(|| uri.path().to_owned(), |p| p.as_str().to_owned()),
    );
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        let reset = chrono::Utc::now().timestamp() + s.rate_limit_reset_secs;
        let mut resp = (
            StatusCode::FORBIDDEN,
            r#"{"message":"API rate limit exceeded"}"#,
        )
            .into_response();
        let h = resp.headers_mut();
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        h.insert(
            "x-ratelimit-reset",
            HeaderValue::from_str(&reset.to_string()).expect("number"),
        );
        return Some(resp);
    }
    let bearer = bearer(headers);
    if bearer != Some(s.token.as_str())
        && caller(s, headers).is_none()
        && !s.installation_token(bearer)
    {
        return Some(
            (StatusCode::UNAUTHORIZED, r#"{"message":"Bad credentials"}"#).into_response(),
        );
    }
    None
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// The [`MockState::users`] entry whose token the request carries.
fn caller<'a>(s: &'a MockState, headers: &HeaderMap) -> Option<&'a MockUser> {
    let token = bearer(headers)?;
    s.users.iter().find(|u| u.token == token)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, r#"{"message":"Not Found"}"#).into_response()
}

fn owner(repo: &Value) -> &str {
    repo["full_name"]
        .as_str()
        .and_then(|n| n.split('/').next())
        .unwrap_or_default()
}

/// One offset page of `items` and the URL of the next page.
fn page_of(
    s: &MockState,
    uri: &Uri,
    params: &HashMap<String, String>,
    items: Vec<Value>,
) -> (Vec<Value>, Option<String>) {
    let per_page = params
        .get("per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .min(s.page_size)
        .max(1);
    let page_no = params
        .get("page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let start = (page_no - 1) * per_page;
    let chunk = items.iter().skip(start).take(per_page).cloned().collect();
    let next = (start + per_page < items.len())
        .then(|| next_url(s, uri, params, &[("page", (page_no + 1).to_string())]));
    (chunk, next)
}

/// `body` as JSON with a `Link` header to `next`.
fn with_link(body: Value, next: Option<String>) -> Response {
    let mut resp = axum::Json(body).into_response();
    if let Some(next) = next {
        resp.headers_mut().insert(
            "link",
            HeaderValue::from_str(&format!("<{next}>; rel=\"next\"")).expect("header"),
        );
    }
    resp
}

/// One offset page of `items` as a JSON array.
fn page(s: &MockState, uri: &Uri, params: &HashMap<String, String>, items: Vec<Value>) -> Response {
    let (chunk, next) = page_of(s, uri, params, items);
    with_link(Value::Array(chunk), next)
}

fn next_url(
    s: &MockState,
    uri: &Uri,
    params: &HashMap<String, String>,
    set: &[(&str, String)],
) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .filter(|(k, _)| !set.iter().any(|(s, _)| s == k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.extend(set.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
    pairs.sort();
    let qs: String = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!("{}{}?{qs}", s.base, uri.path())
}

async fn current_user(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    if s.installation_token(bearer(&headers)) {
        return (
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by integration"}"#,
        )
            .into_response();
    }
    let (id, login, scopes) = match caller(&s, &headers) {
        Some(u) => (u.id, u.login.clone(), u.scopes.clone()),
        None => (s.user_id, s.login.clone(), s.scopes.clone()),
    };
    let mut resp = axum::Json(json!({
        "id": id,
        "login": login,
        "name": s.name,
        "email": s.email,
        "avatar_url": format!("https://avatars.example.com/u/{id}"),
    }))
    .into_response();
    if let Some(scopes) = &scopes {
        resp.headers_mut().insert(
            "x-oauth-scopes",
            HeaderValue::from_str(&scopes.join(", ")).expect("header"),
        );
    }
    resp
}

async fn user_emails(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    axum::Json(Value::Array(s.emails.clone())).into_response()
}

async fn user_orgs(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let items = s.orgs.iter().map(|o| json!({"login": o})).collect();
    page(&s, &uri, &params, items)
}

/// `GET /login/oauth/authorize`: signs in as the mock's user (when [`MockState::authorize`])
/// and redirects to `redirect_uri` with a code, or with `error=access_denied`.
async fn authorize(
    State(state): Shared,
    Query(q): Query<HashMap<String, String>>,
    uri: Uri,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.requests.push(
        uri.path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
    );
    let (Some(redirect), Some(st)) = (q.get("redirect_uri"), q.get("state")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if q.get("client_id") != Some(&s.client_id)
        || q.get("code_challenge_method").map(String::as_str) != Some("S256")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let st: String = url::form_urlencoded::byte_serialize(st.as_bytes()).collect();
    let location = if s.authorize {
        let code = format!("code-{}", s.codes.len() + s.requests.len());
        let challenge = q.get("code_challenge").cloned().unwrap_or_default();
        s.codes.insert(code.clone(), challenge);
        format!("{redirect}?code={code}&state={st}")
    } else {
        format!("{redirect}?error=access_denied&state={st}")
    };
    (StatusCode::FOUND, [("location", location)]).into_response()
}

/// `POST /login/oauth/access_token`: the `authorization_code` grant (with PKCE), issuing the
/// accepted token.
async fn access_token(
    State(state): Shared,
    uri: Uri,
    axum::Form(f): axum::Form<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.requests.push(uri.path().to_owned());
    if f.get("client_id") != Some(&s.client_id) || f.get("client_secret") != Some(&s.client_secret)
    {
        return axum::Json(json!({"error": "incorrect_client_credentials"})).into_response();
    }
    let challenge = f.get("code").and_then(|c| s.codes.remove(c));
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if f.get("grant_type").map(String::as_str) != Some("authorization_code")
        || challenge != Some(sourcegull_crypto::pkce::challenge_s256(&verifier))
    {
        return // GitHub answers a bad code with 200.
        axum::Json(json!({"error": "bad_verification_code"})).into_response();
    }
    axum::Json(json!({
        "access_token": s.token,
        "token_type": "bearer",
        "scope": "read:org,read:user,user:email",
    }))
    .into_response()
}

async fn user_repos(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let readable = caller(&s, &headers).map_or(&s.affiliated, |u| &u.repos);
    let items = s
        .repos
        .iter()
        .filter(|r| r["id"].as_i64().is_some_and(|id| readable.contains(&id)))
        .cloned()
        .collect();
    page(&s, &uri, &params, items)
}

/// `GET /repos/:owner/:name/collaborators`: the [`MockState::users`] who read the repository,
/// with `pull` permission.
async fn collaborators(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    if s.collaborators_forbidden {
        return (
            StatusCode::FORBIDDEN,
            r#"{"message":"Must have push access to view repository collaborators."}"#,
        )
            .into_response();
    }
    let full = format!("{owner}/{name}");
    let Some(id) = s
        .repos
        .iter()
        .find(|r| {
            r["full_name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(&full))
        })
        .and_then(|r| r["id"].as_i64())
    else {
        return not_found();
    };
    let items = s
        .users
        .iter()
        .filter(|u| u.repos.contains(&id))
        .map(|u| {
            json!({
                "id": u.id,
                "login": u.login,
                "permissions": {"admin": false, "push": false, "pull": true},
            })
        })
        .collect();
    page(&s, &uri, &params, items)
}

async fn org_repos(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let items: Vec<Value> = s
        .repos
        .iter()
        .filter(|r| owner(r).eq_ignore_ascii_case(&org))
        .cloned()
        .collect();
    if items.is_empty() {
        return not_found();
    }
    page(&s, &uri, &params, items)
}

async fn get_repo(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let full = format!("{owner}/{name}");
    s.repos
        .iter()
        .find(|r| {
            r["full_name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(&full))
        })
        .map_or_else(not_found, |r| axum::Json(r.clone()).into_response())
}

async fn repo_by_id(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    s.repos
        .iter()
        .find(|r| r["id"].as_i64() == Some(id))
        .map_or_else(not_found, |r| axum::Json(r.clone()).into_response())
}

/// `GET /repositories?since=<id>`: minimal objects of public repositories after `since`.
async fn all_repos(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let since = params
        .get("since")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let mut public: Vec<&Value> = s
        .repos
        .iter()
        .filter(|r| r["private"] == false && r["id"].as_i64().is_some_and(|id| id > since))
        .collect();
    public.sort_by_key(|r| r["id"].as_i64());
    let chunk: Vec<Value> = public
        .iter()
        .take(s.page_size)
        .map(|r| json!({"id": r["id"], "full_name": r["full_name"], "private": false}))
        .collect();
    let next = (public.len() > s.page_size)
        .then(|| chunk.last().and_then(|r| r["id"].as_i64()))
        .flatten()
        .map(|last| next_url(&s, &uri, &params, &[("since", last.to_string())]));
    with_link(Value::Array(chunk), next)
}

async fn search_repos(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let q = params.get("q").cloned().unwrap_or_default();
    let items: Vec<Value> = s
        .repos
        .iter()
        .filter(|r| match q.strip_prefix("org:") {
            Some(org) => owner(r).eq_ignore_ascii_case(org),
            None => r["full_name"].as_str().is_some_and(|n| n.contains(&q)),
        })
        .cloned()
        .collect();
    let total = items.len();
    let (chunk, next) = page_of(&s, &uri, &params, items);
    with_link(
        json!({"total_count": total, "incomplete_results": false, "items": chunk}),
        next,
    )
}

/// `POST /app/installations/:id/access_tokens`: verifies the App JWT (RS256, `iss` the App ID,
/// at most 10 minutes long) and issues an installation token.
async fn installation_access_token(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<u64>,
) -> Response {
    let delay = {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        s.requests.push(uri.path().to_owned());
        s.mint_delay
    };
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(app_id) = s.app_id else {
        return not_found();
    };
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&[app_id.to_string()]);
    validation.set_required_spec_claims(&["exp", "iat", "iss"]);
    let key = jsonwebtoken::DecodingKey::from_rsa_pem(APP_PUBLIC_KEY.as_bytes()).expect("key");
    let claims = bearer(&headers).and_then(|jwt| {
        jsonwebtoken::decode::<crate::app::AppClaims>(jwt, &key, &validation)
            .ok()
            .map(|d| d.claims)
    });
    let now = chrono::Utc::now().timestamp();
    match claims {
        Some(c) if c.exp - now <= 600 && c.iat <= now => {}
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"message":"A JSON web token could not be decoded"}"#,
            )
                .into_response();
        }
    }
    if id != s.installation_id {
        return not_found();
    }
    let token = format!("ghs_installation_{}", s.installation_tokens.len() + 1);
    let expires = now + s.installation_token_ttl_secs;
    s.installation_tokens.push((token.clone(), expires));
    let expires_at = chrono::DateTime::from_timestamp(expires, 0)
        .expect("timestamp")
        .to_rfc3339();
    (
        StatusCode::CREATED,
        axum::Json(json!({"token": token, "expires_at": expires_at})),
    )
        .into_response()
}

/// `GET /installation/repositories`: the [`MockState::affiliated`] repositories, to
/// installation tokens only.
async fn installation_repos(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    if !s.installation_token(bearer(&headers)) {
        return (
            StatusCode::FORBIDDEN,
            r#"{"message":"This endpoint requires an installation token"}"#,
        )
            .into_response();
    }
    let items: Vec<Value> = s
        .repos
        .iter()
        .filter(|r| {
            r["id"]
                .as_i64()
                .is_some_and(|id| s.affiliated.contains(&id))
        })
        .cloned()
        .collect();
    let total = items.len();
    let (chunk, next) = page_of(&s, &uri, &params, items);
    with_link(json!({"total_count": total, "repositories": chunk}), next)
}
