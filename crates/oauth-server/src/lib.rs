//! OAuth 2.1 authorization server for MCP clients, following the MCP authorization
//! specification. MCP clients connect with the server URL alone and sign the user in through
//! the browser; the tokens they get work only at `/.api/mcp`.
//!
//! - **Discovery**: `/.well-known/oauth-protected-resource` (RFC 9728, also under
//!   `/.well-known/oauth-protected-resource/.api/mcp`) names this instance as the
//!   authorization server of the MCP resource; `/.well-known/oauth-authorization-server`
//!   (RFC 8414) lists the endpoints below. The MCP endpoint's `401` points to the first
//!   ([`resource_metadata_url`]).
//! - **Clients** ([`clients`]): Dynamic Client Registration (RFC 7591) at `/.oauth/register`,
//!   rate limited and switchable off (`mcp.oauth.dynamicClientRegistration`), and client ID
//!   metadata documents (an `https` URL as `client_id`, fetched for a signed-in user only,
//!   never from a private address or a host in `webhooks.outbound.allowedHosts`, and cached).
//!   Clients are public: they authenticate with PKCE, not a secret.
//!   Redirect URIs are loopback (any port, RFC 8252) or `https`, restricted to
//!   `mcp.oauth.allowedRedirectHosts` when it is set.
//! - **Authorization** ([`authorize`]): `/.oauth/authorize` checks the request (authorization
//!   code with PKCE `S256` only, resource indicators per RFC 8707) and sends the browser to the
//!   consent page (`/oauth/consent`), which signs the user in if needed and asks them through
//!   `/.api/oauth/authorizations/{request}`.
//! - **Tokens** ([`token`]): `/.oauth/token` (`authorization_code` and `refresh_token`
//!   grants; refresh tokens rotate on use, and presenting a rotated one revokes the whole
//!   grant, as does presenting a used authorization code) and `/.oauth/revoke` (RFC 7009).
//! - **Janitor** ([`janitor`]): a worker singleton deleting expired authorization requests
//!   and unused clients.
//!
//! Every route answers 404 while `mcp.oauth.enabled` is off. Tokens are opaque, stored hashed
//! and bound to the MCP resource; see [`sourcegull_authn::oauth`].

pub mod authorize;
pub mod clients;
mod feature;
pub mod janitor;
pub mod token;

pub use feature::OAuthServerFeature;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sourcegull_authn::oauth::{SCOPE_MCP, canonical_resource};
use sourcegull_services::AppState;

/// Path of the protected resource: the MCP endpoint.
pub const RESOURCE_PATH: &str = "/.api/mcp";
/// The protected resource metadata document.
pub const PROTECTED_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
/// The authorization server metadata document.
pub const AUTHORIZATION_SERVER_METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
/// The authorization endpoint.
pub const AUTHORIZE_PATH: &str = "/.oauth/authorize";
/// The token endpoint.
pub const TOKEN_PATH: &str = "/.oauth/token";
/// The registration endpoint.
pub const REGISTER_PATH: &str = "/.oauth/register";
/// The revocation endpoint.
pub const REVOKE_PATH: &str = "/.oauth/revoke";
/// The consent page of the web app.
pub const CONSENT_PAGE: &str = "/oauth/consent";

/// The OAuth server's routes, to merge into the public router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            PROTECTED_RESOURCE_METADATA_PATH,
            get(protected_resource_metadata).options(preflight),
        )
        .route(
            &format!("{PROTECTED_RESOURCE_METADATA_PATH}{RESOURCE_PATH}"),
            get(protected_resource_metadata).options(preflight),
        )
        .route(
            AUTHORIZATION_SERVER_METADATA_PATH,
            get(authorization_server_metadata).options(preflight),
        )
        .route(AUTHORIZE_PATH, get(rate_limited_authorize))
        .route(
            "/.api/oauth/authorizations/{request}",
            get(authorize::show).post(authorize::answer),
        )
        .route(TOKEN_PATH, post(token::token).options(preflight))
        .route(REVOKE_PATH, post(token::revoke).options(preflight))
        .route(REGISTER_PATH, post(clients::register).options(preflight))
}

/// [`authorize::authorize`], limited per client address
/// (`auth.rateLimits.oauthAuthorizePerAddress`): over the limit it answers 429.
async fn rate_limited_authorize(
    State(state): State<AppState>,
    sourcegull_http_extract::ClientIp(ip): sourcegull_http_extract::ClientIp,
    req: axum::extract::Request,
) -> Response {
    use sourcegull_services::rate_limits::{RateLimit, address};
    if let Err(l) = state
        .rate_limit_hit(RateLimit::OAuthAuthorizePerAddress, &address(ip))
        .await
    {
        return sourcegull_http_extract::too_many_requests(l.retry_after);
    }
    axum::handler::Handler::call(authorize::authorize, req, state).await
}

/// Whether `mcp.oauth.enabled` is on.
pub fn enabled(state: &AppState) -> bool {
    state.config.load().value.mcp_oauth_enabled
}

/// The URL this instance is reached at, without a trailing slash: `externalURL`, or else the
/// request's `Host` (with `https` when `X-Forwarded-Proto` says so). This is the issuer.
pub fn base_url(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(url) = state.config.load().value.external_url.as_deref() {
        return Some(url.trim_end_matches('/').to_owned());
    }
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if host.is_empty() || host.contains(['/', ' ', '@']) {
        return None;
    }
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    Some(format!("{}://{host}", if https { "https" } else { "http" }))
}

/// The canonical URL of the MCP resource (see [`canonical_resource`]).
pub fn resource_url(state: &AppState, headers: &HeaderMap) -> Option<String> {
    canonical_resource(&format!("{}{RESOURCE_PATH}", base_url(state, headers)?))
}

/// The URL of the protected resource metadata, for the `resource_metadata` parameter of the
/// MCP endpoint's `WWW-Authenticate` challenge. `None` while OAuth is off.
pub fn resource_metadata_url(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if !enabled(state) {
        return None;
    }
    Some(format!(
        "{}{PROTECTED_RESOURCE_METADATA_PATH}{RESOURCE_PATH}",
        base_url(state, headers)?
    ))
}

async fn protected_resource_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !enabled(&state) {
        return not_found();
    }
    let (Some(issuer), Some(resource)) =
        (base_url(&state, &headers), resource_url(&state, &headers))
    else {
        return no_base_url();
    };
    public_json(json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "scopes_supported": [SCOPE_MCP],
        "bearer_methods_supported": ["header"],
        "resource_name": "Sourcegull",
    }))
}

async fn authorization_server_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !enabled(&state) {
        return not_found();
    }
    let Some(issuer) = base_url(&state, &headers) else {
        return no_base_url();
    };
    let mut doc = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}{AUTHORIZE_PATH}"),
        "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
        "revocation_endpoint": format!("{issuer}{REVOKE_PATH}"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE_MCP],
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": true,
    });
    if state
        .config
        .load()
        .value
        .mcp_oauth_dynamic_client_registration
    {
        doc["registration_endpoint"] = json!(format!("{issuer}{REGISTER_PATH}"));
    }
    public_json(doc)
}

/// Answers CORS preflights of the endpoints browser-based clients call. No credentials are
/// ever allowed cross-origin.
async fn preflight(headers: HeaderMap) -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    allow_any_origin(h);
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    let requested = headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    resp
}

fn allow_any_origin(h: &mut HeaderMap) {
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
}

/// A JSON response readable cross-origin, never cached.
pub(crate) fn public_json_status(status: StatusCode, v: Value) -> Response {
    let mut resp = (status, Json(v)).into_response();
    let h = resp.headers_mut();
    allow_any_origin(h);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    resp
}

pub(crate) fn public_json(v: Value) -> Response {
    public_json_status(StatusCode::OK, v)
}

/// An OAuth error response (`{"error", "error_description"}`).
pub(crate) fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    public_json_status(
        status,
        json!({ "error": error, "error_description": description }),
    )
}

pub(crate) fn not_found() -> Response {
    oauth_error(
        StatusCode::NOT_FOUND,
        "not_found",
        "OAuth sign-in for MCP clients is turned off (mcp.oauth.enabled).",
    )
}

pub(crate) fn no_base_url() -> Response {
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Set externalURL in the site configuration.",
    )
}

/// Logs an internal error and answers `server_error`.
pub(crate) fn server_error(context: &str, err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "{context}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "Internal error.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_path_is_the_mcp_endpoint() {
        assert_eq!(RESOURCE_PATH, "/.api/mcp");
    }
}
