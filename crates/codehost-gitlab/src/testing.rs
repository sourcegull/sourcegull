//! A mock GitLab API server for tests (feature `testing`).
//!
//! Serves the endpoints discovery uses from an in-memory project list: `GET /api/v4/version`,
//! `/personal_access_tokens/self`, `/projects` (keyset and offset pagination,
//! `last_activity_after`), `/projects/:id_or_path` and `/groups/:group/projects`. Pages are
//! small (default 2 items) so pagination is exercised. Every request is recorded, and the next
//! N requests can be made to fail with 429 to test rate limit handling.
//!
//! For sign-in and permissions it also has users ([`MockUser`]) with project
//! memberships and groups: `GET /oauth/authorize` (signs in as [`MockState::authorize_as`] and
//! redirects back with a code), `POST /oauth/token` (code with PKCE, refresh token), `GET
//! /api/v4/user`, `/users?username=`, `/groups?min_access_level=` and
//! `/projects/:id/members/all`. Requests act as the user of a user token, or as the `Sudo`
//! user with the admin token; `/projects` then honours `min_access_level` and `visibility`.
//!
//! Groups: with the admin token, `GET /api/v4/groups` lists [`MockState::group_tree`]
//! and `GET /api/v4/groups/:id/members` the users whose [`MockUser::groups`] hold the group's
//! full path (direct members).
//!
//! Project hooks: `GET|POST /api/v4/projects/:id/hooks` and `PUT|DELETE
//! /api/v4/projects/:id/hooks/:hook` on [`MockState::hooks`] (the stored hooks keep the
//! `token` sent, which `GET` leaves out as GitLab does). Writes are recorded in
//! [`MockState::hook_writes`]; projects in [`MockState::hooks_forbidden`] answer 403.
//!
//! CI job tokens: `GET /api/v4/job` with `JOB-TOKEN` answers the job of a token in
//! [`MockState::job_tokens`] (401 otherwise, 500 while [`MockState::job_error`] is set).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// The mutable state behind a [`MockGitLab`].
#[derive(Debug)]
pub struct MockState {
    /// Projects as GitLab returns them.
    pub projects: Vec<Value>,
    /// Group full path → ids of its projects.
    pub groups: HashMap<String, Vec<i64>>,
    /// Every group of the instance, as the admin token lists them.
    pub group_tree: Vec<MockGroup>,
    /// `GET /version` result; `None` answers 404.
    pub version: Option<String>,
    /// The accepted token (`PRIVATE-TOKEN` or bearer).
    pub token: String,
    /// `GET /personal_access_tokens/self` scopes; `None` answers 404.
    pub scopes: Option<Vec<String>>,
    /// Maximum page size served.
    pub page_size: usize,
    /// The next this-many API requests get `429 Retry-After: <rate_limit_retry_after>`.
    pub rate_limited: u32,
    /// The `Retry-After` seconds of rate-limited responses (default 0).
    pub rate_limit_retry_after: u32,
    /// Every request's path and query, in order.
    pub requests: Vec<String>,
    /// Users, for sign-in and permissions.
    pub users: Vec<MockUser>,
    /// The OAuth application.
    pub client_id: String,
    pub client_secret: String,
    /// The user `GET /oauth/authorize` signs in as.
    pub authorize_as: Option<i64>,
    /// Lifetime of issued access tokens.
    pub token_ttl_secs: i64,
    /// Valid user access tokens → user id.
    pub user_tokens: HashMap<String, i64>,
    /// Valid refresh tokens → user id.
    pub refresh_tokens: HashMap<String, i64>,
    /// CI job tokens of running jobs → the job's project id.
    pub job_tokens: HashMap<String, i64>,
    /// CI job tokens → (job id, ref, username) of the job; without an entry the job is
    /// `1000 + project id` on `main`, triggered by `ci-user`.
    pub job_details: HashMap<String, (i64, String, String)>,
    /// `GET /job` answers 500.
    pub job_error: bool,
    /// Project id → its hooks (with the `token` they were saved with).
    pub hooks: BTreeMap<i64, Vec<Value>>,
    /// Projects whose hooks answer 403 (the token is not a Maintainer).
    pub hooks_forbidden: HashSet<i64>,
    /// Hook writes in order, as `"<METHOD> <project>"`.
    pub hook_writes: Vec<String>,
    /// Authorization codes → (user id, PKCE challenge).
    codes: HashMap<String, (i64, String)>,
    issued: u64,
    base: String,
}

/// A GitLab user of the mock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockUser {
    pub id: i64,
    pub username: String,
    pub email: String,
    /// Project id → access level (10 guest, 20 reporter, 30 developer, …).
    pub projects: BTreeMap<i64, i64>,
    /// Full paths of the user's groups.
    pub groups: Vec<String>,
}

impl MockUser {
    /// A user with no memberships.
    pub fn new(id: i64, username: &str) -> Self {
        Self {
            id,
            username: username.to_owned(),
            email: format!("{username}@example.com"),
            projects: BTreeMap::new(),
            groups: Vec::new(),
        }
    }

    /// Adds a project membership.
    pub fn member(mut self, project: i64, access_level: i64) -> Self {
        self.projects.insert(project, access_level);
        self
    }
}

/// A group of the mock instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockGroup {
    pub id: i64,
    pub full_path: String,
    pub name: String,
    pub parent_id: Option<i64>,
}

impl MockGroup {
    /// A group named after the last segment of its path.
    pub fn new(id: i64, full_path: &str, parent_id: Option<i64>) -> Self {
        Self {
            id,
            full_path: full_path.to_owned(),
            name: full_path.rsplit('/').next().unwrap_or(full_path).to_owned(),
            parent_id,
        }
    }
}

/// A running mock server; stopped when dropped.
pub struct MockGitLab {
    /// Base URL, e.g. `http://127.0.0.1:41234`.
    pub url: String,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockGitLab {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A project object with the fields discovery reads. `id` also determines `star_count`.
pub fn project(id: i64, path: &str, last_activity_at: DateTime<Utc>) -> Value {
    json!({
        "id": id,
        "path_with_namespace": path,
        "description": format!("project {id}"),
        "web_url": format!("https://gitlab.example.com/{path}"),
        "http_url_to_repo": format!("https://gitlab.example.com/{path}.git"),
        "ssh_url_to_repo": format!("git@gitlab.example.com:{path}.git"),
        "default_branch": "main",
        "visibility": "private",
        "archived": false,
        "star_count": id,
        "forks_count": 0,
        "topics": [],
        "last_activity_at": last_activity_at,
        "created_at": "2025-01-01T00:00:00Z",
        "empty_repo": false,
        "forked_from_project": null,
    })
}

impl MockGitLab {
    /// Starts a server on a free local port accepting `token`.
    pub async fn start(token: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitLab");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(MockState {
            projects: Vec::new(),
            groups: HashMap::new(),
            group_tree: Vec::new(),
            version: Some("16.5.1-ee".into()),
            token: token.to_owned(),
            scopes: Some(vec!["read_api".into()]),
            page_size: 2,
            rate_limited: 0,
            rate_limit_retry_after: 0,
            requests: Vec::new(),
            users: Vec::new(),
            client_id: "mock-client".into(),
            client_secret: "mock-secret".into(),
            authorize_as: None,
            token_ttl_secs: 7200,
            user_tokens: HashMap::new(),
            refresh_tokens: HashMap::new(),
            job_tokens: HashMap::new(),
            job_details: HashMap::new(),
            job_error: false,
            hooks: BTreeMap::new(),
            hooks_forbidden: HashSet::new(),
            hook_writes: Vec::new(),
            codes: HashMap::new(),
            issued: 0,
            base: url.clone(),
        }));
        let app = Router::new()
            .route("/api/v4/version", get(version))
            .route("/api/v4/personal_access_tokens/self", get(token_self))
            .route("/api/v4/projects", get(list_projects))
            .route("/api/v4/projects/{id}", get(get_project))
            .route("/api/v4/groups/{group}/projects", get(group_projects))
            .route("/api/v4/groups/{group}/members", get(group_members))
            .route("/api/v4/projects/{id}/members/all", get(project_members))
            .route(
                "/api/v4/projects/{id}/hooks",
                get(list_hooks).post(create_hook),
            )
            .route(
                "/api/v4/projects/{id}/hooks/{hook}",
                axum::routing::put(update_hook).delete(delete_hook),
            )
            .route("/api/v4/user", get(current_user))
            .route("/api/v4/job", get(current_job))
            .route("/api/v4/users", get(find_users))
            .route("/api/v4/groups", get(user_groups))
            .route("/oauth/authorize", get(authorize))
            .route("/oauth/token", axum::routing::post(oauth_token))
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

/// Who a request acts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acting {
    /// The admin token without `Sudo`.
    Admin,
    /// A user (user token, or admin token with `Sudo`).
    User(i64),
}

impl MockState {
    fn user(&self, id: i64) -> Option<&MockUser> {
        self.users.iter().find(|u| u.id == id)
    }
}

fn user_json(u: &MockUser) -> Value {
    json!({
        "id": u.id,
        "username": u.username,
        "name": format!("User {}", u.username),
        "email": u.email,
        "avatar_url": format!("https://gitlab.example.com/avatar/{}", u.id),
        "state": "active",
        "confirmed_at": "2025-01-01T00:00:00Z",
    })
}

type Shared = State<Arc<Mutex<MockState>>>;

/// `GET /job`: the job of a CI job token.
async fn current_job(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.requests.push(uri.path().to_owned());
    if s.job_error {
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    let token = headers
        .get("job-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match s.job_tokens.get(token).copied() {
        Some(project_id) => {
            let (id, git_ref, username) = s
                .job_details
                .get(token)
                .cloned()
                .unwrap_or_else(|| (1000 + project_id, "main".into(), "ci-user".into()));
            axum::Json(json!({
                "id": id,
                "status": "running",
                "ref": git_ref,
                "user": { "id": 1, "username": username },
                "pipeline": { "id": 2000 + project_id, "project_id": project_id, "status": "running" },
            }))
            .into_response()
        }
        None => (
            StatusCode::UNAUTHORIZED,
            r#"{"message":"401 Unauthorized"}"#,
        )
            .into_response(),
    }
}

/// Records the request and applies auth and rate limiting. `Err` is the response to send.
#[allow(clippy::result_large_err)]
fn admit(state: &Arc<Mutex<MockState>>, headers: &HeaderMap, uri: &Uri) -> Result<(), Response> {
    admit_as(state, headers, uri).map(|_| ())
}

/// [`admit`], returning who the request acts as.
#[allow(clippy::result_large_err)]
fn admit_as(
    state: &Arc<Mutex<MockState>>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Acting, Response> {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.requests.push(
        uri.path_and_query()
            .map(ToString::to_string)
            .unwrap_or_default(),
    );
    let given = headers
        .get("private-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
        });
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            r#"{"message":"401 Unauthorized"}"#,
        )
            .into_response()
    };
    let sudo = headers.get("sudo").and_then(|v| v.to_str().ok());
    let acting = match given.as_deref() {
        Some(t) if t == s.token => match sudo {
            None => Acting::Admin,
            Some(who) => {
                let found = s
                    .users
                    .iter()
                    .find(|u| u.id.to_string() == who || u.username.eq_ignore_ascii_case(who));
                match found {
                    Some(u) => Acting::User(u.id),
                    None => {
                        return Err(
                            (StatusCode::NOT_FOUND, r#"{"message":"404 User Not Found"}"#)
                                .into_response(),
                        );
                    }
                }
            }
        },
        Some(t) => match s.user_tokens.get(t) {
            Some(&id) if sudo.is_none() => Acting::User(id),
            Some(_) => {
                return Err((StatusCode::FORBIDDEN, "sudo needs an admin token").into_response());
            }
            None => return Err(unauthorized()),
        },
        None => return Err(unauthorized()),
    };
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", s.rate_limit_retry_after.to_string())],
            "Retry later",
        )
            .into_response());
    }
    Ok(acting)
}

async fn version(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let v = state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .version
        .clone();
    match v {
        Some(v) => axum::Json(json!({ "version": v, "revision": "mock" })).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn token_self(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let scopes = state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .scopes
        .clone();
    match scopes {
        Some(scopes) => axum::Json(json!({ "scopes": scopes, "active": true })).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_project(
    State(state): Shared,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let found = s.projects.iter().find(|p| {
        p["id"].as_i64().map(|i| i.to_string()).as_deref() == Some(id.as_str())
            || p["path_with_namespace"].as_str() == Some(id.as_str())
    });
    match found {
        Some(p) => axum::Json(p.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            r#"{"message":"404 Project Not Found"}"#,
        )
            .into_response(),
    }
}

async fn list_projects(
    State(state): Shared,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let acting = match admit_as(&state, &headers, &uri) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let min_level = q
        .get("min_access_level")
        .and_then(|v| v.parse::<i64>().ok());
    let user = match acting {
        Acting::User(id) => s.user(id),
        Acting::Admin => None,
    };
    let all = s
        .projects
        .iter()
        .filter(|p| {
            q.get("visibility")
                .is_none_or(|v| p["visibility"].as_str() == Some(v.as_str()))
        })
        .filter(|p| match (min_level, user) {
            (Some(min), Some(u)) => p["id"]
                .as_i64()
                .and_then(|id| u.projects.get(&id))
                .is_some_and(|&level| level >= min),
            _ => true,
        })
        .cloned()
        .collect();
    page(&s, "/api/v4/projects", all, &q)
}

async fn project_members(
    State(state): Shared,
    Path(id): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let Ok(project) = id.parse::<i64>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let members: Vec<Value> = s
        .users
        .iter()
        .filter_map(|u| {
            u.projects.get(&project).map(|level| {
                let mut j = user_json(u);
                j["access_level"] = json!(level);
                j
            })
        })
        .collect();
    page(
        &s,
        &format!("/api/v4/projects/{project}/members/all"),
        members,
        &q,
    )
}

async fn current_user(State(state): Shared, headers: HeaderMap, uri: Uri) -> Response {
    let acting = match admit_as(&state, &headers, &uri) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    match acting {
        Acting::User(id) => match s.user(id) {
            Some(u) => axum::Json(user_json(u)).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Acting::Admin => {
            axum::Json(json!({"id": 1, "username": "root", "state": "active"})).into_response()
        }
    }
}

async fn find_users(
    State(state): Shared,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let users: Vec<Value> = s
        .users
        .iter()
        .filter(|u| {
            q.get("username")
                .is_none_or(|n| u.username.eq_ignore_ascii_case(n))
        })
        .map(user_json)
        .collect();
    axum::Json(users).into_response()
}

async fn user_groups(
    State(state): Shared,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let acting = match admit_as(&state, &headers, &uri) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let groups: Vec<Value> = match acting {
        Acting::User(id) => s
            .user(id)
            .map(|u| {
                u.groups
                    .iter()
                    .enumerate()
                    .map(|(i, g)| json!({"id": i + 1, "full_path": g}))
                    .collect()
            })
            .unwrap_or_default(),
        Acting::Admin => s
            .group_tree
            .iter()
            .map(|g| {
                json!({
                    "id": g.id,
                    "full_path": g.full_path,
                    "path": g.full_path.rsplit('/').next(),
                    "name": g.name,
                    "parent_id": g.parent_id,
                })
            })
            .collect(),
    };
    page(&s, "/api/v4/groups", groups, &q)
}

/// `GET /groups/:id/members`: the users that list the group (by full path) directly.
async fn group_members(
    State(state): Shared,
    Path(group): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(g) = s
        .group_tree
        .iter()
        .find(|g| g.id.to_string() == group || g.full_path == group)
    else {
        return (
            StatusCode::NOT_FOUND,
            r#"{"message":"404 Group Not Found"}"#,
        )
            .into_response();
    };
    let members: Vec<Value> = s
        .users
        .iter()
        .filter(|u| u.groups.contains(&g.full_path))
        .map(|u| {
            let mut j = user_json(u);
            j["access_level"] = json!(30);
            j
        })
        .collect();
    let path = format!("/api/v4/groups/{}/members", g.id);
    page(&s, &path, members, &q)
}

/// `GET /oauth/authorize`: signs in as [`MockState::authorize_as`] and redirects to
/// `redirect_uri` with a code (or `error=access_denied` when nobody is set).
async fn authorize(
    State(state): Shared,
    Query(q): Query<BTreeMap<String, String>>,
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
    let location = match s.authorize_as {
        Some(user) => {
            s.issued += 1;
            let code = format!("code-{}", s.issued);
            let challenge = q.get("code_challenge").cloned().unwrap_or_default();
            s.codes.insert(code.clone(), (user, challenge));
            format!("{redirect}?code={code}&state={st}")
        }
        None => format!("{redirect}?error=access_denied&state={st}"),
    };
    (StatusCode::FOUND, [("location", location)]).into_response()
}

/// `POST /oauth/token`: the `authorization_code` (with PKCE) and `refresh_token` grants.
async fn oauth_token(
    State(state): Shared,
    uri: Uri,
    axum::Form(f): axum::Form<BTreeMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.requests.push(uri.path().to_owned());
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant"})),
        )
            .into_response()
    };
    if f.get("client_id") != Some(&s.client_id) || f.get("client_secret") != Some(&s.client_secret)
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let user = match f.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let Some((user, challenge)) = f.get("code").and_then(|c| s.codes.remove(c)) else {
                return invalid();
            };
            let verifier = f.get("code_verifier").cloned().unwrap_or_default();
            if sourcegull_crypto::pkce::challenge_s256(&verifier) != challenge {
                return invalid();
            }
            user
        }
        Some("refresh_token") => {
            match f
                .get("refresh_token")
                .and_then(|r| s.refresh_tokens.remove(r))
            {
                Some(user) => user,
                None => return invalid(),
            }
        }
        _ => return invalid(),
    };
    s.issued += 1;
    let access = format!("user-token-{user}-{}", s.issued);
    let refresh = format!("refresh-{user}-{}", s.issued);
    s.user_tokens.insert(access.clone(), user);
    s.refresh_tokens.insert(refresh.clone(), user);
    axum::Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": s.token_ttl_secs,
        "refresh_token": refresh,
        "scope": "read_user read_api",
        "created_at": Utc::now().timestamp(),
    }))
    .into_response()
}

async fn group_projects(
    State(state): Shared,
    Path(group): Path<String>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit(&state, &headers, &uri) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(ids) = s.groups.get(&group) else {
        return (
            StatusCode::NOT_FOUND,
            r#"{"message":"404 Group Not Found"}"#,
        )
            .into_response();
    };
    let projects = s
        .projects
        .iter()
        .filter(|p| p["id"].as_i64().is_some_and(|id| ids.contains(&id)))
        .cloned()
        .collect();
    let path = format!(
        "/api/v4/groups/{}/projects",
        url::form_urlencoded::byte_serialize(group.as_bytes()).collect::<String>()
    );
    page(&s, &path, projects, &q)
}

/// Serves one page of `projects` (filtered by `last_activity_after`) with a `Link` header.
/// Other lists without `last_activity_at` or numeric ids page the same way.
fn page(
    s: &MockState,
    path: &str,
    mut projects: Vec<Value>,
    q: &BTreeMap<String, String>,
) -> Response {
    if let Some(after) = q
        .get("last_activity_after")
        .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
    {
        projects.retain(|p| {
            p["last_activity_at"]
                .as_str()
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .is_some_and(|t| t > after)
        });
    }
    projects.sort_by_key(|p| p["id"].as_i64());
    let per_page = q
        .get("per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20)
        .min(s.page_size)
        .max(1);
    let mut next_query = q.clone();
    let (items, more) = if q.get("pagination").map(String::as_str) == Some("keyset") {
        let after = q
            .get("id_after")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let rest: Vec<Value> = projects
            .into_iter()
            .filter(|p| p["id"].as_i64().unwrap_or(0) > after)
            .collect();
        let more = rest.len() > per_page;
        let items: Vec<Value> = rest.into_iter().take(per_page).collect();
        if let Some(last) = items.last().and_then(|p| p["id"].as_i64()) {
            next_query.insert("id_after".into(), last.to_string());
        }
        (items, more)
    } else {
        let page = q
            .get("page")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let start = (page - 1) * per_page;
        let more = projects.len() > start + per_page;
        let items = projects.into_iter().skip(start).take(per_page).collect();
        next_query.insert("page".into(), (page + 1).to_string());
        (items, more)
    };
    let mut resp = axum::Json(Value::Array(items)).into_response();
    if more {
        let qs: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(next_query.iter())
            .finish();
        let link = format!("<{}{path}?{qs}>; rel=\"next\"", s.base);
        if let Ok(v) = link.parse() {
            resp.headers_mut().insert("link", v);
        }
    }
    resp
}

/// Admits a hook request for project `id`: 403 for [`MockState::hooks_forbidden`], 404 for
/// a project that is neither in [`MockState::projects`] nor in [`MockState::hooks`].
#[allow(clippy::result_large_err)]
fn admit_hooks(
    state: &Arc<Mutex<MockState>>,
    headers: &HeaderMap,
    uri: &Uri,
    id: i64,
) -> Result<(), Response> {
    admit(state, headers, uri)?;
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    if s.hooks_forbidden.contains(&id) {
        return Err((StatusCode::FORBIDDEN, r#"{"message":"403 Forbidden"}"#).into_response());
    }
    if !s.hooks.contains_key(&id) && !s.projects.iter().any(|p| p["id"].as_i64() == Some(id)) {
        return Err((
            StatusCode::NOT_FOUND,
            r#"{"message":"404 Project Not Found"}"#,
        )
            .into_response());
    }
    Ok(())
}

/// A stored hook as `GET` shows it (without its token).
fn shown_hook(h: &Value) -> Value {
    let mut h = h.clone();
    if let Some(o) = h.as_object_mut() {
        o.remove("token");
    }
    h
}

async fn list_hooks(
    State(state): Shared,
    Path(id): Path<i64>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit_hooks(&state, &headers, &uri, id) {
        return r;
    }
    let s = state.lock().unwrap_or_else(PoisonError::into_inner);
    let hooks: Vec<Value> = s
        .hooks
        .get(&id)
        .map(|h| h.iter().map(shown_hook).collect())
        .unwrap_or_default();
    axum::Json(hooks).into_response()
}

async fn create_hook(
    State(state): Shared,
    Path(id): Path<i64>,
    headers: HeaderMap,
    uri: Uri,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if let Err(r) = admit_hooks(&state, &headers, &uri, id) {
        return r;
    }
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.hook_writes.push(format!("POST {id}"));
    let next = s
        .hooks
        .values()
        .flatten()
        .filter_map(|h| h["id"].as_i64())
        .max()
        .unwrap_or(0)
        + 1;
    let mut hook = body;
    hook["id"] = json!(next);
    hook["project_id"] = json!(id);
    s.hooks.entry(id).or_default().push(hook.clone());
    (StatusCode::CREATED, axum::Json(shown_hook(&hook))).into_response()
}

async fn update_hook(
    State(state): Shared,
    Path((id, hook)): Path<(i64, i64)>,
    headers: HeaderMap,
    uri: Uri,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    if let Err(r) = admit_hooks(&state, &headers, &uri, id) {
        return r;
    }
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.hook_writes.push(format!("PUT {id}"));
    let found = s
        .hooks
        .get_mut(&id)
        .and_then(|hs| hs.iter_mut().find(|h| h["id"].as_i64() == Some(hook)));
    match found {
        Some(h) => {
            if let (Some(o), Some(b)) = (h.as_object_mut(), body.as_object()) {
                for (k, v) in b {
                    o.insert(k.clone(), v.clone());
                }
            }
            axum::Json(shown_hook(h)).into_response()
        }
        None => (StatusCode::NOT_FOUND, r#"{"message":"404 Not found"}"#).into_response(),
    }
}

async fn delete_hook(
    State(state): Shared,
    Path((id, hook)): Path<(i64, i64)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Err(r) = admit_hooks(&state, &headers, &uri, id) {
        return r;
    }
    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
    s.hook_writes.push(format!("DELETE {id}"));
    if let Some(hs) = s.hooks.get_mut(&id) {
        hs.retain(|h| h["id"].as_i64() != Some(hook));
    }
    StatusCode::NO_CONTENT.into_response()
}
