//! SCIM 2.0 user provisioning at `/.api/scim/v2` (RFC 7643, RFC 7644).
//!
//! Identity providers (Okta, Microsoft Entra ID, Keycloak, …) create, update, deactivate and
//! delete Sourcegull users here, authenticated with `Authorization: Bearer <scim.authToken>`.
//! Without `scim.authToken` in the site config every endpoint answers 404.
//!
//! - `GET /Users` (`filter`, `startIndex`, `count`, `attributes`, `excludedAttributes`),
//!   `POST /Users/.search`, `POST /Users`, `GET`/`PUT`/`PATCH`/`DELETE /Users/{id}`.
//! - `GET /ServiceProviderConfig`, `/ResourceTypes[/User]`, `/Schemas[/{urn}]`.
//!
//! Behaviour: users created through SCIM get the provider's addresses
//! as verified emails and a `scim` external account holding the SCIM attributes. Creating a
//! user whose verified email belongs to an existing user who is not SCIM-controlled takes
//! that user over instead. `active: false` soft-deletes the user (sessions end, access
//! tokens are revoked, sign-in fails) and `active: true` restores them; `DELETE`
//! deletes the user permanently. Every listing includes all live users, so providers can
//! match existing accounts, plus deactivated SCIM users. Responses carry weak ETags
//! (`meta.version`); `If-Match` on writes and `If-None-Match` on reads are honoured.

mod filter;
mod patch;
mod resource;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use filter::{AttrPath, Filter};
use resource::{ERROR_SCHEMA, LIST_RESPONSE_SCHEMA, MAX_RESULTS};
use serde_json::{Value, json};
use sourcegull_db::{DbError, SCIM_USER_NAME_CONSTRAINT, ScimNarrow, ScimUser, ScimWritten};
use sourcegull_services::AppState;
use sourcegull_types::UserId;

/// Path prefix of the SCIM API.
pub const SCIM_PREFIX: &str = "/.api/scim/v2";
/// Media type of SCIM requests and responses.
pub const SCIM_CONTENT_TYPE: &str = "application/scim+json";
/// Resources a list returns when the request has no `count`.
const DEFAULT_COUNT: i64 = 100;

/// Security events recorded for SCIM writes.
pub mod events {
    pub const USER_CREATED: &str = "SCIMUserCreated";
    pub const USER_UPDATED: &str = "SCIMUserUpdated";
    pub const USER_DEACTIVATED: &str = "SCIMUserDeactivated";
    pub const USER_DELETED: &str = "SCIMUserDeleted";
}

/// The SCIM routes, merged into [`public_routes`](crate::public_routes).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/.api/scim/v2/ServiceProviderConfig",
            get(service_provider_config),
        )
        .route("/.api/scim/v2/ResourceTypes", get(resource_types))
        .route("/.api/scim/v2/ResourceTypes/{id}", get(resource_type))
        .route("/.api/scim/v2/Schemas", get(schemas))
        .route("/.api/scim/v2/Schemas/{id}", get(schema))
        .route("/.api/scim/v2/Users", get(list_users).post(create_user))
        .route("/.api/scim/v2/Users/.search", post(search_users))
        .route(
            "/.api/scim/v2/Users/{id}",
            get(get_user)
                .put(replace_user)
                .patch(patch_user)
                .delete(delete_user),
        )
        .route("/.api/scim/v2", any(unknown_endpoint))
        .route("/.api/scim/v2/{*rest}", any(unknown_endpoint))
}

/// A SCIM error response (RFC 7644 §3.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScimError {
    pub status: StatusCode,
    pub scim_type: Option<&'static str>,
    pub detail: String,
}

impl ScimError {
    fn new(status: StatusCode, scim_type: Option<&'static str>, detail: impl Into<String>) -> Self {
        Self {
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    pub(crate) fn invalid_syntax(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidSyntax"), detail)
    }

    pub(crate) fn invalid_value(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
    }

    pub(crate) fn invalid_path(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidPath"), detail)
    }

    pub(crate) fn invalid_filter(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("invalidFilter"), detail)
    }

    pub(crate) fn no_target(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("noTarget"), detail)
    }

    pub(crate) fn mutability(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, Some("mutability"), detail)
    }

    fn uniqueness(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, Some("uniqueness"), detail)
    }

    fn not_found(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            None,
            format!("Resource {id} not found"),
        )
    }

    pub(crate) fn internal(detail: impl std::fmt::Display) -> Self {
        tracing::error!(error = %detail, "SCIM request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "internal server error",
        )
    }
}

impl From<DbError> for ScimError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::AlreadyExists { constraint } => {
                let detail = if constraint == SCIM_USER_NAME_CONSTRAINT {
                    "a user with this userName already exists"
                } else if constraint.contains("account") {
                    "a user with this externalId already exists"
                } else if constraint.contains("email") {
                    "another user has verified one of these email addresses"
                } else if constraint.contains("username") {
                    "no free username is left for this userName"
                } else {
                    "the user conflicts with an existing one"
                };
                Self::uniqueness(detail)
            }
            DbError::NotFound(what) => {
                Self::new(StatusCode::NOT_FOUND, None, format!("{what} not found"))
            }
            other => Self::internal(other),
        }
    }
}

impl IntoResponse for ScimError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "schemas": [ERROR_SCHEMA],
            "status": self.status.as_u16().to_string(),
            "detail": self.detail,
        });
        if let Some(t) = self.scim_type {
            body["scimType"] = Value::String(t.into());
        }
        let mut resp = scim_json(self.status, &body);
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"sourcegull-scim\""),
            );
        }
        resp
    }
}

type Result<T, E = ScimError> = std::result::Result<T, E>;

/// A JSON response with the SCIM media type.
fn scim_json(status: StatusCode, body: &Value) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(SCIM_CONTENT_TYPE),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Extractor admitting requests that carry the configured bearer token.
#[derive(Debug, Clone, Copy)]
pub struct ScimAuth;

impl FromRequestParts<AppState> for ScimAuth {
    type Rejection = ScimError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self> {
        let config = state.config.load();
        let Some(expected) = config
            .value
            .scim_auth_token
            .as_deref()
            .filter(|t| !t.is_empty())
        else {
            return Err(ScimError::new(
                StatusCode::NOT_FOUND,
                None,
                "SCIM provisioning is not enabled (set scim.authToken in the site configuration)",
            ));
        };
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim());
        match token {
            Some(t) if sourcegull_crypto::constant_time_eq(t.as_bytes(), expected.as_bytes()) => {
                Ok(Self)
            }
            _ => Err(ScimError::new(
                StatusCode::UNAUTHORIZED,
                None,
                "a valid bearer token is required",
            )),
        }
    }
}

/// The absolute SCIM base URL (`externalURL` + prefix), or the bare prefix without one.
fn base_url(state: &AppState) -> String {
    let config = state.config.load();
    let external = config.value.external_url.as_deref().unwrap_or_default();
    format!("{}{SCIM_PREFIX}", external.trim_end_matches('/'))
}

fn parse_body(body: &Bytes) -> Result<Value> {
    serde_json::from_slice(body)
        .map_err(|e| ScimError::invalid_syntax(format!("invalid JSON: {e}")))
}

fn parse_id(id: &str) -> Result<UserId> {
    id.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(UserId)
        .ok_or_else(|| ScimError::not_found(id))
}

async fn service_provider_config(_: ScimAuth, State(state): State<AppState>) -> Response {
    scim_json(
        StatusCode::OK,
        &resource::service_provider_config(&base_url(&state)),
    )
}

fn list_response(total: usize, resources: Vec<Value>) -> Response {
    scim_json(
        StatusCode::OK,
        &json!({
            "schemas": [LIST_RESPONSE_SCHEMA],
            "totalResults": total,
            "startIndex": 1,
            "itemsPerPage": resources.len(),
            "Resources": resources,
        }),
    )
}

async fn resource_types(_: ScimAuth, State(state): State<AppState>) -> Response {
    list_response(1, vec![resource::user_resource_type(&base_url(&state))])
}

async fn resource_type(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response> {
    if id == "User" {
        Ok(scim_json(
            StatusCode::OK,
            &resource::user_resource_type(&base_url(&state)),
        ))
    } else {
        Err(ScimError::not_found(&id))
    }
}

async fn schemas(_: ScimAuth, State(state): State<AppState>) -> Response {
    let all = resource::schemas(&base_url(&state));
    list_response(all.len(), all)
}

async fn schema(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response> {
    resource::schemas(&base_url(&state))
        .into_iter()
        .find(|s| {
            s["id"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&id))
        })
        .map(|s| scim_json(StatusCode::OK, &s))
        .ok_or_else(|| ScimError::not_found(&id))
}

async fn unknown_endpoint(_: ScimAuth) -> ScimError {
    ScimError::new(
        StatusCode::NOT_FOUND,
        None,
        "unknown SCIM endpoint (only Users is supported)",
    )
}

/// Query parameters of a list (RFC 7644 §3.4.2).
#[derive(Debug, Default)]
struct ListParams {
    filter: Option<Filter>,
    start_index: i64,
    count: i64,
    attributes: Vec<AttrPath>,
    excluded: Vec<AttrPath>,
}

fn attr_list(raw: &[String]) -> Result<Vec<AttrPath>> {
    raw.iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| AttrPath::parse(s).map_err(ScimError::invalid_value))
        .collect()
}

impl ListParams {
    fn new(
        filter: Option<&str>,
        start_index: Option<i64>,
        count: Option<i64>,
        attributes: &[String],
        excluded: &[String],
    ) -> Result<Self> {
        let filter = match filter.map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => Some(filter::parse(f).map_err(ScimError::invalid_filter)?),
            None => None,
        };
        Ok(Self {
            filter,
            // Values below 1 mean 1; negative counts mean 0 (RFC 7644 §3.4.2.4).
            start_index: start_index.unwrap_or(1).max(1),
            count: count.unwrap_or(DEFAULT_COUNT).clamp(0, MAX_RESULTS),
            attributes: attr_list(attributes)?,
            excluded: attr_list(excluded)?,
        })
    }

    fn from_query(query: &[(String, String)]) -> Result<Self> {
        let get = |name: &str| {
            query
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        let int =
            |name: &str| -> Result<Option<i64>> {
                match get(name).first() {
                    None => Ok(None),
                    Some(v) => v.trim().parse().map(Some).map_err(|_| {
                        ScimError::invalid_value(format!("{name} must be an integer"))
                    }),
                }
            };
        Self::new(
            get("filter").first().map(String::as_str),
            int("startIndex")?,
            int("count")?,
            &get("attributes"),
            &get("excludedAttributes"),
        )
    }
}

async fn list_users(
    _: ScimAuth,
    State(state): State<AppState>,
    Query(query): Query<Vec<(String, String)>>,
) -> Result<Response> {
    let params = ListParams::from_query(&query)?;
    list(&state, params).await
}

/// `POST /Users/.search` (RFC 7644 §3.4.3).
async fn search_users(_: ScimAuth, State(state): State<AppState>, body: Bytes) -> Result<Response> {
    let body = parse_body(&body)?;
    let strings = |key: &str| -> Vec<String> {
        match filter::get_ci(&body, key) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        }
    };
    let int = |key: &str| filter::get_ci(&body, key).and_then(Value::as_i64);
    let params = ListParams::new(
        filter::get_ci(&body, "filter").and_then(Value::as_str),
        int("startIndex"),
        int("count"),
        &strings("attributes"),
        &strings("excludedAttributes"),
    )?;
    list(&state, params).await
}

async fn list(state: &AppState, params: ListParams) -> Result<Response> {
    let base = base_url(state);
    let store = state.db.scim();
    let offset = params.start_index - 1;
    let (total, page): (usize, Vec<Value>) = match &params.filter {
        None => {
            let (total, users) = store
                .list(&ScimNarrow::default(), offset, Some(params.count))
                .await?;
            let page = users.iter().map(|u| resource::render(u, &base)).collect();
            (usize::try_from(total).unwrap_or_default(), page)
        }
        Some(f) => {
            // Narrow in SQL, then evaluate the exact filter on the rendered resources.
            let (_, users) = store.list(&filter::narrow(f), 0, None).await?;
            let matching: Vec<Value> = users
                .iter()
                .map(|u| resource::render(u, &base))
                .filter(|r| filter::matches(f, r))
                .collect();
            let total = matching.len();
            let page = matching
                .into_iter()
                .skip(usize::try_from(offset).unwrap_or_default())
                .take(usize::try_from(params.count).unwrap_or_default())
                .collect();
            (total, page)
        }
    };
    let resources: Vec<Value> = page
        .into_iter()
        .map(|r| resource::project(r, &params.attributes, &params.excluded))
        .collect();
    Ok(scim_json(
        StatusCode::OK,
        &json!({
            "schemas": [LIST_RESPONSE_SCHEMA],
            "totalResults": total,
            "startIndex": params.start_index,
            "itemsPerPage": resources.len(),
            "Resources": resources,
        }),
    ))
}

/// A single resource with its `ETag` (and `Location` when created).
fn resource_response(status: StatusCode, resource: Value) -> Response {
    let version = resource::version_of(&resource).to_owned();
    let location = resource
        .pointer("/meta/location")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut resp = scim_json(status, &resource);
    if let Ok(v) = HeaderValue::from_str(&version) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    if status == StatusCode::CREATED
        && let Some(v) = location.and_then(|l| HeaderValue::from_str(&l).ok())
    {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp
}

fn strip_weak(tag: &str) -> &str {
    let tag = tag.trim();
    tag.strip_prefix("W/").unwrap_or(tag)
}

/// Whether an `If-Match` / `If-None-Match` header lists `version` (or `*`).
fn tag_listed(headers: &HeaderMap, name: header::HeaderName, version: &str) -> Option<bool> {
    let value = headers.get(name)?.to_str().ok()?;
    Some(
        value
            .split(',')
            .any(|t| t.trim() == "*" || strip_weak(t) == strip_weak(version)),
    )
}

/// Fails with 412 when `If-Match` is present and does not list the current version.
fn check_if_match(headers: &HeaderMap, current: &Value) -> Result<()> {
    if tag_listed(headers, header::IF_MATCH, resource::version_of(current)) == Some(false) {
        return Err(ScimError::new(
            StatusCode::PRECONDITION_FAILED,
            None,
            "the resource has changed (If-Match does not match its version)",
        ));
    }
    Ok(())
}

async fn load(state: &AppState, id: &str) -> Result<ScimUser> {
    let user_id = parse_id(id)?;
    state
        .db
        .scim()
        .get(user_id)
        .await?
        .ok_or_else(|| ScimError::not_found(id))
}

async fn get_user(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Result<Response> {
    let params = ListParams::from_query(&query)?;
    let user = load(&state, &id).await?;
    let r = resource::render(&user, &base_url(&state));
    if tag_listed(&headers, header::IF_NONE_MATCH, resource::version_of(&r)) == Some(true) {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(v) = HeaderValue::from_str(resource::version_of(&r)) {
            resp.headers_mut().insert(header::ETAG, v);
        }
        return Ok(resp);
    }
    Ok(resource_response(
        StatusCode::OK,
        resource::project(r, &params.attributes, &params.excluded),
    ))
}

async fn create_user(_: ScimAuth, State(state): State<AppState>, body: Bytes) -> Result<Response> {
    let attrs = parse_body(&body)?;
    let w = resource::to_write(&attrs, None)?;
    let store = state.db.scim();
    // A user who already verified one of the addresses is taken over, unless the provider
    // manages them already (then this is a duplicate).
    let target = match store
        .users_with_verified_emails(&w.emails)
        .await?
        .as_slice()
    {
        [] => None,
        [id] => {
            let existing = store.get(*id).await?;
            if existing.is_some_and(|u| u.account.is_some()) {
                return Err(ScimError::uniqueness(
                    "a user with this email address already exists",
                ));
            }
            Some(*id)
        }
        _ => {
            return Err(ScimError::uniqueness(
                "the email addresses belong to several users",
            ));
        }
    };
    let written = store.write(target, &w).await?;
    after_write(&state, &written, events::USER_CREATED).await;
    let user = load(&state, &written.user_id.to_string()).await?;
    Ok(resource_response(
        StatusCode::CREATED,
        resource::render(&user, &base_url(&state)),
    ))
}

async fn replace_user(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let attrs = parse_body(&body)?;
    let current = load(&state, &id).await?;
    check_if_match(&headers, &resource::render(&current, &base_url(&state)))?;
    let w = resource::to_write(&attrs, Some(&current))?;
    update(&state, &current, &w).await
}

async fn patch_user(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let ops = parse_body(&body)?;
    let current = load(&state, &id).await?;
    let mut r = resource::render(&current, &base_url(&state));
    check_if_match(&headers, &r)?;
    patch::apply(&mut r, &ops)?;
    let w = resource::to_write(&r, Some(&current))?;
    update(&state, &current, &w).await
}

async fn update(
    state: &AppState,
    current: &ScimUser,
    w: &sourcegull_db::ScimWrite,
) -> Result<Response> {
    let written = state.db.scim().write(Some(current.user.id), w).await?;
    let event = if written.deactivated {
        events::USER_DEACTIVATED
    } else {
        events::USER_UPDATED
    };
    after_write(state, &written, event).await;
    let user = load(state, &written.user_id.to_string()).await?;
    Ok(resource_response(
        StatusCode::OK,
        resource::render(&user, &base_url(state)),
    ))
}

async fn delete_user(
    _: ScimAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response> {
    let current = load(&state, &id).await?;
    check_if_match(&headers, &resource::render(&current, &base_url(&state)))?;
    if current.account.is_none() {
        return Err(ScimError::mutability(
            "the user is not managed through SCIM; deactivate or take it over with PUT first",
        ));
    }
    if !state.db.scim().hard_delete(current.user.id).await? {
        return Err(ScimError::not_found(&id));
    }
    end_sessions(&state, current.user.id).await;
    state
        .auth
        .log_event(
            events::USER_DELETED,
            None,
            json!({ "userID": current.user.id.get(), "username": current.user.username }),
        )
        .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Drops cached user info and, after a deactivation, ends sessions held by the session store
/// and revokes OAuth grants.
async fn after_write(state: &AppState, written: &ScimWritten, event: &str) {
    if written.deactivated {
        end_sessions(state, written.user_id).await;
    } else {
        state.auth.invalidate_user(written.user_id).await;
    }
    state
        .auth
        .log_event(event, Some(written.user_id), json!({ "source": "scim" }))
        .await;
}

async fn end_sessions(state: &AppState, user: UserId) {
    if let Err(e) = state.auth.sessions().delete_for_user(user).await {
        tracing::warn!(error = %e, user = user.get(), "ending sessions of a SCIM-deactivated user failed");
    }
    state
        .auth
        .revoke_oauth_grants_for_user(user, "user deactivated")
        .await;
    state.auth.invalidate_user(user).await;
}
