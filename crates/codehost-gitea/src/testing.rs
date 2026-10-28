//! A mock Gitea API for tests (feature `testing`).
//!
//! Serves the endpoints discovery uses under `/api/v1` from an in-memory repository list:
//! `GET /user`, `/version`, `/user/repos` (repositories in [`MockState::affiliated`]),
//! `/orgs/:org/repos` (repositories whose owner is the org), `/repos/:owner/:name`,
//! `/repositories/:id` and `/repos/search` (the keyword `q` is matched as a substring of
//! `full_name`; without one every repository is listed). Pages are small (default 2 items,
//! whatever `limit` asks for) so pagination is exercised, with `X-Total-Count` unless
//! [`MockState::total_count`] is off. For sign-in it also serves `/user/emails`, `/user/orgs`
//! and the OAuth endpoints `GET /login/oauth/authorize` and `POST /login/oauth/access_token`
//! (the `authorization_code` grant issues the accepted token, the `refresh_token` grant a new
//! user token from [`MockState::refresh_tokens`]).
//!
//! For repository permissions it has users ([`MockUser`]), per-repository collaborators and
//! organization teams ([`MockTeam`]), and serves `GET /users/:name`, `/orgs/:org` (an
//! organization exists when it has a team), `/repos/:owner/:name/collaborators`,
//! `/repos/:owner/:name/teams` (organizations only, unpaginated) and `/teams/:id/members`.
//! Requests act as a user with a user token ([`MockGitea::issue_token`]) or with the accepted
//! token plus `Sudo: <login>`; `GET /user/repos` then lists the repositories the user owns,
//! collaborates on or reaches through a team, with `permissions.pull` saying whether the user
//! reads code.
//!
//! Every request is recorded, and the next N requests can be made to fail with a rate limit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

/// The mutable state behind a [`MockGitea`].
#[derive(Debug)]
pub struct MockState {
    /// Repositories as Gitea returns them.
    pub repos: Vec<Value>,
    /// Ids of the repositories `GET /user/repos` lists.
    pub affiliated: HashSet<i64>,
    /// The accepted token.
    pub token: String,
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
    /// `GET /version`.
    pub version: String,
    /// Maximum page size served.
    pub page_size: usize,
    /// Whether lists send `X-Total-Count`.
    pub total_count: bool,
    /// The next this-many API requests get `429` with `Retry-After:`
    /// [`retry_after_secs`](Self::retry_after_secs).
    pub rate_limited: u32,
    /// `Retry-After` of rate-limited responses (default 0).
    pub retry_after_secs: u64,
    /// Every request's path and query, in order.
    pub requests: Vec<String>,
    /// Users of the instance (permissions).
    pub users: Vec<MockUser>,
    /// Repository id → ids of its collaborators.
    pub collaborators: BTreeMap<i64, Vec<i64>>,
    /// Organization teams.
    pub teams: Vec<MockTeam>,
    /// User access tokens → user id.
    pub user_tokens: HashMap<String, i64>,
    /// User access tokens without the `read:repository` scope: repository endpoints answer
    /// `403` for them.
    pub scopeless_tokens: HashSet<String>,
    /// Refresh tokens → user id (the `refresh_token` grant).
    pub refresh_tokens: HashMap<String, i64>,
    /// Tokens issued so far (for unique token names).
    pub issued: u32,
}

/// A user of the mock instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockUser {
    pub id: i64,
    pub login: String,
    /// `false` for a deactivated user.
    pub active: bool,
}

impl MockUser {
    /// An active user.
    pub fn new(id: i64, login: &str) -> Self {
        Self {
            id,
            login: login.to_owned(),
            active: true,
        }
    }
}

/// A team of an organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockTeam {
    pub id: i64,
    pub org: String,
    pub name: String,
    /// `none`, `read`, `write`, `admin` or `owner`.
    pub permission: String,
    /// Access to the code unit (`units_map["repo.code"]`), when set.
    pub code: Option<String>,
    /// Ids of the repositories the team has.
    pub repos: Vec<i64>,
    /// Ids of the team's members.
    pub members: Vec<i64>,
}

impl MockTeam {
    /// A team with `permission` and no repositories or members.
    pub fn new(id: i64, org: &str, permission: &str) -> Self {
        Self {
            id,
            org: org.to_owned(),
            name: format!("team{id}"),
            permission: permission.to_owned(),
            code: None,
            repos: Vec::new(),
            members: Vec::new(),
        }
    }

    /// Whether members read code.
    fn reads_code(&self) -> bool {
        let reads = |m: &str| matches!(m, "read" | "write" | "admin" | "owner");
        matches!(self.permission.as_str(), "admin" | "owner")
            || self.code.as_deref().map_or(reads(&self.permission), reads)
    }
}

/// Who a request acts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acting {
    /// The accepted token without `Sudo`.
    Token,
    /// A user, by user token or `Sudo`.
    User(i64),
}

/// A running mock server; stopped when dropped.
pub struct MockGitea {
    /// Base URL, e.g. `http://127.0.0.1:41234` (the API is under `/api/v1`).
    pub url: String,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockGitea {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A repository object with the fields discovery reads, owned by a public user or org. `id`
/// also determines `stars_count`.
pub fn repo(id: i64, full_name: &str) -> Value {
    let owner = full_name.split('/').next().unwrap_or_default();
    json!({
        "id": id,
        "owner": {"id": id + 1000, "login": owner, "visibility": "public"},
        "name": full_name.split('/').nth(1).unwrap_or_default(),
        "full_name": full_name,
        "description": format!("repo {id}"),
        "html_url": format!("https://gitea.example.com/{full_name}"),
        "clone_url": format!("https://gitea.example.com/{full_name}.git"),
        "ssh_url": format!("git@gitea.example.com:{full_name}.git"),
        "default_branch": "main",
        "private": true,
        "internal": false,
        "archived": false,
        "fork": false,
        "mirror": false,
        "template": false,
        "empty": false,
        "stars_count": id,
        "forks_count": 0,
        "topics": [],
        "size": 10,
        "updated_at": "2026-01-01T00:00:00Z",
        "created_at": "2025-01-01T00:00:00Z",
        "permissions": {"admin": false, "push": false, "pull": true},
    })
}

impl MockGitea {
    /// Starts a server on a free local port accepting `token`.
    pub async fn start(token: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock Gitea");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(MockState {
            repos: Vec::new(),
            affiliated: HashSet::new(),
            token: token.to_owned(),
            login: "alice".into(),
            user_id: 1,
            name: None,
            email: None,
            emails: Vec::new(),
            orgs: Vec::new(),
            client_id: "mock-client".into(),
            client_secret: "mock-secret".into(),
            authorize: true,
            codes: HashMap::new(),
            version: "1.24.6".into(),
            page_size: 2,
            total_count: true,
            rate_limited: 0,
            retry_after_secs: 0,
            requests: Vec::new(),
            users: Vec::new(),
            collaborators: BTreeMap::new(),
            teams: Vec::new(),
            user_tokens: HashMap::new(),
            scopeless_tokens: HashSet::new(),
            refresh_tokens: HashMap::new(),
            issued: 0,
        }));
        let app = Router::new()
            .route("/api/v1/user", get(current_user))
            .route("/api/v1/user/emails", get(user_emails))
            .route("/api/v1/user/orgs", get(user_orgs))
            .route("/login/oauth/authorize", get(authorize))
            .route(
                "/login/oauth/access_token",
                axum::routing::post(access_token),
            )
            .route("/api/v1/version", get(version))
            .route("/api/v1/user/repos", get(user_repos))
            .route("/api/v1/orgs/{org}/repos", get(org_repos))
            .route("/api/v1/repos/search", get(search_repos))
            .route("/api/v1/repos/{owner}/{name}", get(get_repo))
            .route("/api/v1/repositories/{id}", get(repo_by_id))
            .route("/api/v1/users/{name}", get(user_by_name))
            .route("/api/v1/orgs/{org}", get(get_org))
            .route(
                "/api/v1/repos/{owner}/{name}/collaborators",
                get(repo_collaborators),
            )
            .route("/api/v1/repos/{owner}/{name}/teams", get(repo_teams))
            .route("/api/v1/teams/{id}/members", get(team_members))
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

    /// The recorded requests (path and query).
    pub fn requests(&self) -> Vec<String> {
        self.with(|s| s.requests.clone())
    }

    /// Issues an access token for user `id` (as if the user had signed in), returning it.
    pub fn issue_token(&self, id: i64) -> String {
        self.with(|s| {
            s.issued += 1;
            let t = format!("user-token-{id}-{}", s.issued);
            s.user_tokens.insert(t.clone(), id);
            t
        })
    }
}

type Shared = State<Arc<Mutex<MockState>>>;

/// Records the request and checks the token and the rate limit; `Some` is the refusal to send.
fn admit(s: &mut MockState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    admit_as(s, headers, uri).err()
}

/// [`admit`], returning who the request acts as.
#[allow(clippy::result_large_err)]
fn admit_as(s: &mut MockState, headers: &HeaderMap, uri: &Uri) -> Result<Acting, Response> {
    s.requests.push(
        uri.path_and_query()
            .map_or_else(|| uri.path().to_owned(), |p| p.as_str().to_owned()),
    );
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        let mut resp = (StatusCode::TOO_MANY_REQUESTS, "Too Many Requests").into_response();
        resp.headers_mut().insert(
            "retry-after",
            HeaderValue::from_str(&s.retry_after_secs.to_string()).expect("number"),
        );
        return Err(resp);
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("token "));
    let sudo = headers.get("sudo").and_then(|v| v.to_str().ok());
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            r#"{"message":"user does not exist"}"#,
        )
            .into_response()
    };
    match token {
        Some(t) if t == s.token => match sudo {
            None => Ok(Acting::Token),
            Some(who) => s
                .users
                .iter()
                .find(|u| u.login.eq_ignore_ascii_case(who))
                .map(|u| Acting::User(u.id))
                .ok_or_else(not_found),
        },
        Some(t) => match s.user_tokens.get(t) {
            Some(_) if s.scopeless_tokens.contains(t) && uri.path().contains("/repos") => Err((
                StatusCode::FORBIDDEN,
                r#"{"message":"token does not have at least one of required scope(s): [read:repository]"}"#,
            )
                .into_response()),
            Some(&id) if sudo.is_none() => Ok(Acting::User(id)),
            Some(_) => Err((StatusCode::FORBIDDEN, "sudo needs an admin token").into_response()),
            None => Err(unauthorized()),
        },
        None => Err(unauthorized()),
    }
}

impl MockState {
    fn user_json(&self, id: i64) -> Option<Value> {
        self.users.iter().find(|u| u.id == id).map(
            |u| json!({"id": u.id, "login": u.login, "active": u.active, "prohibit_login": false}),
        )
    }

    fn repo_named(&self, owner: &str, name: &str) -> Option<&Value> {
        let full = format!("{owner}/{name}");
        self.repos.iter().find(|r| {
            r["full_name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(&full))
        })
    }

    /// Whether `user` is involved with repository `repo` (lists it), and whether they read its
    /// code.
    fn access(&self, user: i64, repo: &Value) -> Option<bool> {
        let id = repo["id"].as_i64()?;
        let login = &self.users.iter().find(|u| u.id == user)?.login;
        if owner(repo).eq_ignore_ascii_case(login)
            || self
                .collaborators
                .get(&id)
                .is_some_and(|c| c.contains(&user))
        {
            return Some(true);
        }
        let teams: Vec<&MockTeam> = self
            .teams
            .iter()
            .filter(|t| t.repos.contains(&id) && t.members.contains(&user))
            .collect();
        (!teams.is_empty()).then(|| teams.iter().any(|t| t.reads_code()))
    }
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

/// One page of `items`, by `page` and at most [`MockState::page_size`] long.
fn page_of(s: &MockState, params: &HashMap<String, String>, items: Vec<Value>) -> Vec<Value> {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .min(s.page_size)
        .max(1);
    let page_no = params
        .get("page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    items
        .into_iter()
        .skip((page_no - 1) * limit)
        .take(limit)
        .collect()
}

/// `body` as JSON, with `X-Total-Count: total` when the mock sends it.
fn with_total(s: &MockState, body: Value, total: usize) -> Response {
    let mut resp = axum::Json(body).into_response();
    if s.total_count {
        resp.headers_mut().insert(
            "x-total-count",
            HeaderValue::from_str(&total.to_string()).expect("number"),
        );
    }
    resp
}

/// One page of `items` as a JSON array.
fn page(s: &MockState, params: &HashMap<String, String>, items: Vec<Value>) -> Response {
    let total = items.len();
    let chunk = page_of(s, params, items);
    with_total(s, Value::Array(chunk), total)
}

async fn current_user(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    axum::Json(json!({
        "id": s.user_id,
        "login": s.login,
        "full_name": s.name.clone().unwrap_or_default(),
        "email": s.email.clone().unwrap_or_default(),
        "avatar_url": format!("https://gitea.example.com/avatars/{}", s.user_id),
        "is_admin": false,
    }))
    .into_response()
}

async fn version(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    axum::Json(json!({"version": s.version})).into_response()
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
    let items = s
        .orgs
        .iter()
        .map(|o| json!({"name": o, "username": o}))
        .collect();
    page(&s, &params, items)
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
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    if f.get("grant_type").map(String::as_str) == Some("refresh_token") {
        let Some(user) = f
            .get("refresh_token")
            .and_then(|r| s.refresh_tokens.remove(r))
        else {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": "invalid_grant"})),
            )
                .into_response();
        };
        s.issued += 1;
        let access = format!("user-token-{user}-{}", s.issued);
        let refresh = format!("refresh-{user}-{}", s.issued);
        s.user_tokens.insert(access.clone(), user);
        s.refresh_tokens.insert(refresh.clone(), user);
        return axum::Json(json!({
            "access_token": access,
            "token_type": "bearer",
            "expires_in": 3600,
            "refresh_token": refresh,
        }))
        .into_response();
    }
    let challenge = f.get("code").and_then(|c| s.codes.remove(c));
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if f.get("grant_type").map(String::as_str) != Some("authorization_code")
        || challenge != Some(sourcegull_crypto::pkce::challenge_s256(&verifier))
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    axum::Json(json!({
        "access_token": s.token,
        "token_type": "bearer",
        "expires_in": 3600,
        "refresh_token": "mock-refresh",
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
    let acting = match admit_as(&mut s, &headers, &uri) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let items = match acting {
        Acting::Token => s
            .repos
            .iter()
            .filter(|r| {
                r["id"]
                    .as_i64()
                    .is_some_and(|id| s.affiliated.contains(&id))
            })
            .cloned()
            .collect(),
        Acting::User(user) => s
            .repos
            .iter()
            .filter_map(|r| {
                let pull = s.access(user, r)?;
                let mut r = r.clone();
                r["permissions"] = json!({"admin": false, "push": false, "pull": pull});
                Some(r)
            })
            .collect(),
    };
    page(&s, &params, items)
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
    page(&s, &params, items)
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
    s.repo_named(&owner, &name)
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
        .filter(|r| r["full_name"].as_str().is_some_and(|n| n.contains(&q)))
        .cloned()
        .collect();
    let total = items.len();
    let chunk = page_of(&s, &params, items);
    with_total(&s, json!({"ok": true, "data": chunk}), total)
}

async fn user_by_name(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    s.users
        .iter()
        .find(|u| u.login.eq_ignore_ascii_case(&name))
        .and_then(|u| s.user_json(u.id))
        .map_or_else(not_found, |u| axum::Json(u).into_response())
}

async fn get_org(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<String>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    if s.teams.iter().any(|t| t.org.eq_ignore_ascii_case(&org)) {
        axum::Json(json!({"name": org, "username": org})).into_response()
    } else {
        not_found()
    }
}

async fn repo_collaborators(
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
    let Some(id) = s.repo_named(&owner, &name).and_then(|r| r["id"].as_i64()) else {
        return not_found();
    };
    let items = s
        .collaborators
        .get(&id)
        .into_iter()
        .flatten()
        .filter_map(|u| s.user_json(*u))
        .collect();
    page(&s, &params, items)
}

/// `GET /repos/:owner/:name/teams`: every team at once; `405` for a repository owned by a
/// user.
async fn repo_teams(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let Some(id) = s.repo_named(&owner, &name).and_then(|r| r["id"].as_i64()) else {
        return not_found();
    };
    if !s.teams.iter().any(|t| t.org.eq_ignore_ascii_case(&owner)) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            r#"{"message":"repo is not owned by an organization"}"#,
        )
            .into_response();
    }
    let teams: Vec<Value> = s
        .teams
        .iter()
        .filter(|t| t.repos.contains(&id))
        .map(|t| {
            let units = t
                .code
                .as_ref()
                .map_or(Value::Null, |c| json!({"repo.code": c}));
            json!({"id": t.id, "name": t.name, "permission": t.permission, "units_map": units})
        })
        .collect();
    axum::Json(Value::Array(teams)).into_response()
}

async fn team_members(
    State(state): Shared,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(r) = admit(&mut s, &headers, &uri) {
        return r;
    }
    let Some(team) = s.teams.iter().find(|t| t.id == id) else {
        return not_found();
    };
    let items = team
        .members
        .iter()
        .filter_map(|u| s.user_json(*u))
        .collect();
    page(&s, &params, items)
}
