//! The authorization endpoint and the consent API.
//!
//! `GET /.oauth/authorize` checks the client and its redirect URI first; until both are
//! known good, errors are shown on the consent page (`/oauth/consent?error=…`), never sent to
//! the redirect URI. A client ID metadata document is fetched only for a signed-in user: an
//! anonymous browser is sent to `/oauth/consent?authorize=<query>`, which signs the user in
//! and comes back with the same query. Then it checks the rest of the request (response type `code`, a PKCE
//! `S256` challenge, scope `mcp`, the MCP `resource`), errors going back to the client, stores
//! the request under a random handle and sends the browser to `/oauth/consent?request=<handle>`.
//!
//! The consent page signs the user in if needed and reads the request with
//! `GET /.api/oauth/authorizations/{handle}`; the user's answer is
//! `POST /.api/oauth/authorizations/{handle}` with `{"approve": true|false}`, which returns
//! the URL to send the browser to: the redirect URI with a one-minute, single-use `code` (or
//! `error=access_denied`), the client's `state` and the issuer (`iss`, RFC 9207). Both need a
//! session: a token cannot approve on a user's behalf, and the POST is subject to the CSRF
//! rule of cookie-authenticated requests.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sourcegull_authn::AuthVia;
use sourcegull_authn::oauth::{SCOPE_MCP, canonical_resource};
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::NewOAuthAuthorization;
use sourcegull_http_extract::{MaybeActor, error_response};
use sourcegull_services::AppState;
use url::Url;

use crate::clients::{self, ClientError, check_redirect_uri, redirect_uri_matches};
use crate::{CONSENT_PAGE, base_url, enabled, no_base_url, not_found, resource_url};

/// How long the user has to sign in and answer.
pub const REQUEST_TTL: chrono::Duration = chrono::Duration::minutes(10);
/// How long an authorization code is valid.
pub const CODE_TTL: chrono::Duration = chrono::Duration::seconds(60);

/// Query parameters, rejecting repeated ones (RFC 6749 §3.1).
pub(crate) fn parse_params(raw: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for (k, v) in url::form_urlencoded::parse(raw) {
        if out.insert(k.to_string(), v.into_owned()).is_some() {
            return Err(format!("The parameter {k} is repeated."));
        }
    }
    Ok(out)
}

/// Whether `challenge` is a well-formed `S256` code challenge (base64url of a SHA-256).
pub fn valid_code_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The scopes of a `scope` parameter: only `mcp`, which is also the default.
pub fn parse_scopes(scope: Option<&str>) -> Result<Vec<String>, String> {
    let mut scopes: Vec<String> = Vec::new();
    for s in scope.unwrap_or_default().split_ascii_whitespace() {
        if s != SCOPE_MCP {
            return Err(format!(
                "Unknown scope {s:?}; the only scope is {SCOPE_MCP}."
            ));
        }
        if !scopes.iter().any(|x| x == s) {
            scopes.push(s.to_owned());
        }
    }
    if scopes.is_empty() {
        scopes.push(SCOPE_MCP.to_owned());
    }
    Ok(scopes)
}

/// `uri` with `params` appended to its query.
pub(crate) fn with_params(uri: &str, params: &[(&str, &str)]) -> String {
    let Ok(mut url) = Url::parse(uri) else {
        return uri.to_owned();
    };
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    url.to_string()
}

fn redirect(to: &str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Shows `message` on the consent page.
fn error_page(message: &str) -> Response {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("error", message)
        .finish();
    redirect(&format!("{CONSENT_PAGE}?{query}"))
}

/// Sends an anonymous browser to the consent page with the request's query as `authorize`:
/// the page signs the user in and then sends the browser back to `/.oauth/authorize` with it.
fn sign_in_first(query: &str) -> Response {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("authorize", query)
        .finish();
    redirect(&format!("{CONSENT_PAGE}?{query}"))
}

/// `GET /.oauth/authorize`.
pub async fn authorize(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if !enabled(&state) {
        return not_found();
    }
    let (Some(issuer), Some(resource)) =
        (base_url(&state, &headers), resource_url(&state, &headers))
    else {
        return no_base_url();
    };
    let query = query.unwrap_or_default();
    let params = match parse_params(query.as_bytes()) {
        Ok(p) => p,
        Err(e) => return error_page(&e),
    };
    let get = |k: &str| params.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let Some(client_id) = get("client_id") else {
        return error_page("The request has no client_id.");
    };
    let client = match clients::resolve(&state, client_id, actor.is_some()).await {
        Ok(c) => c,
        Err(ClientError::Invalid(m)) => return error_page(&m),
        Err(ClientError::SignInRequired) => return sign_in_first(&query),
        Err(ClientError::Internal(e)) => {
            tracing::error!(error = %e, "resolving an OAuth client");
            return error_page("Internal error.");
        }
    };
    let redirect_uri = match get("redirect_uri") {
        Some(r)
            if client
                .redirect_uris
                .iter()
                .any(|reg| redirect_uri_matches(reg, r)) =>
        {
            r.to_owned()
        }
        Some(_) => return error_page("The redirect_uri is not registered for this client."),
        None => match client.redirect_uris.as_slice() {
            [only] => only.clone(),
            _ => return error_page("The request has no redirect_uri."),
        },
    };
    let allowed = state
        .config
        .load()
        .value
        .mcp_oauth_allowed_redirect_hosts
        .clone();
    if let Err(e) = check_redirect_uri(&redirect_uri, &allowed) {
        return error_page(&e);
    }

    let client_state = get("state");
    let fail = |error: &str, description: &str| {
        let mut p = vec![("error", error), ("error_description", description)];
        if let Some(s) = client_state {
            p.push(("state", s));
        }
        p.push(("iss", issuer.as_str()));
        redirect(&with_params(&redirect_uri, &p))
    };
    if get("response_type") != Some("code") {
        return fail(
            "unsupported_response_type",
            "Only the authorization code flow (response_type=code) is supported.",
        );
    }
    let Some(challenge) = get("code_challenge") else {
        return fail("invalid_request", "PKCE is required: send code_challenge.");
    };
    if get("code_challenge_method") != Some("S256") {
        return fail(
            "invalid_request",
            "Only the S256 code_challenge_method is supported.",
        );
    }
    if !valid_code_challenge(challenge) {
        return fail("invalid_request", "The code_challenge is malformed.");
    }
    let scopes = match parse_scopes(get("scope")) {
        Ok(s) => s,
        Err(e) => return fail("invalid_scope", &e),
    };
    if let Some(r) = get("resource")
        && canonical_resource(r).as_deref() != Some(resource.as_str())
    {
        return fail(
            "invalid_target",
            &format!("Tokens of this server are for the MCP endpoint {resource}."),
        );
    }

    let handle = generate_session_id();
    let created = state
        .db
        .oauth()
        .create_authorization(&NewOAuthAuthorization {
            request_hash: &hash_session_id(&handle),
            client_id: client.id,
            redirect_uri: &redirect_uri,
            state: client_state,
            code_challenge: challenge,
            resource: &resource,
            scopes: &scopes,
            expires_at: Utc::now() + REQUEST_TTL,
        })
        .await;
    if let Err(e) = created {
        tracing::error!(error = %e, "storing an OAuth authorization request");
        return fail("server_error", "Internal error.");
    }
    redirect(&format!("{CONSENT_PAGE}?request={handle}"))
}

/// The signed-in user answering, from a session (not a token).
fn session_user(
    actor: Option<sourcegull_authn::Actor>,
) -> Result<sourcegull_authn::Actor, Response> {
    match actor {
        Some(a) if a.via == AuthVia::Session => Ok(a),
        Some(_) => Err(error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "Authorize apps from a browser session.",
        )),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "You must be signed in.",
        )),
    }
}

fn gone() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "This authorization request has expired or was already answered. Start again from the app.",
    )
}

/// `GET /.api/oauth/authorizations/{request}`: what the consent page shows.
pub async fn show(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Path(request): Path<String>,
) -> Response {
    if !enabled(&state) {
        return not_found();
    }
    let actor = match session_user(actor) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let store = state.db.oauth();
    let pending = match store
        .pending_authorization(&hash_session_id(&request))
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => return gone(),
        Err(e) => return internal(e),
    };
    let client = match store.client_by_id(pending.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return gone(),
        Err(e) => return internal(e),
    };
    let redirect = Url::parse(&pending.redirect_uri).ok();
    let redirect_host = redirect
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    // A registered client's name is whatever its registration said, so the consent page warns
    // when the code would leave the user's machine.
    let redirect_warning = client.kind == sourcegull_db::OAuthClientKind::Registered
        && !redirect.as_ref().is_some_and(clients::is_loopback);
    no_store(Json(json!({
        "clientID": client.client_id,
        "clientName": client.client_name,
        "clientURI": client.client_uri,
        "metadataDocument": client.kind == sourcegull_db::OAuthClientKind::MetadataDocument,
        "redirectURI": pending.redirect_uri,
        "redirectHost": redirect_host,
        "redirectWarning": redirect_warning,
        "scopes": pending.scopes,
        "resource": pending.resource,
        "username": actor.username,
    })))
}

/// The consent page's answer.
#[derive(Debug, Deserialize)]
pub struct Answer {
    /// Whether the user allowed access.
    pub approve: bool,
}

/// `POST /.api/oauth/authorizations/{request}`: approve or deny.
pub async fn answer(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    Path(request): Path<String>,
    body: Bytes,
) -> Response {
    if !enabled(&state) {
        return not_found();
    }
    let actor = match session_user(actor) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let Some(issuer) = base_url(&state, &headers) else {
        return no_base_url();
    };
    let Ok(Answer { approve }) = serde_json::from_slice(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "Send {\"approve\": true} or {\"approve\": false}.",
        );
    };
    let store = state.db.oauth();
    let hash = hash_session_id(&request);
    let (authorization, params) = if approve {
        let code = generate_session_id();
        match store
            .approve_authorization(
                &hash,
                actor.user_id,
                &hash_session_id(&code),
                Utc::now() + CODE_TTL,
            )
            .await
        {
            Ok(Some(a)) => (a, vec![("code".to_owned(), code)]),
            Ok(None) => return gone(),
            Err(e) => return internal(e),
        }
    } else {
        match store.deny_authorization(&hash).await {
            Ok(Some(a)) => (
                a,
                vec![
                    ("error".to_owned(), "access_denied".to_owned()),
                    (
                        "error_description".to_owned(),
                        "The user denied access.".to_owned(),
                    ),
                ],
            ),
            Ok(None) => return gone(),
            Err(e) => return internal(e),
        }
    };
    let mut p: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    if let Some(s) = authorization.state.as_deref() {
        p.push(("state", s));
    }
    p.push(("iss", issuer.as_str()));
    no_store(Json(json!({
        "redirectURL": with_params(&authorization.redirect_uri, &p),
    })))
}

fn internal(e: sourcegull_db::DbError) -> Response {
    tracing::error!(error = %e, "OAuth authorization request");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Internal error.",
    )
}

fn no_store(body: impl IntoResponse) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_reject_repeats() {
        let p = parse_params(b"a=1&b=x%20y").unwrap();
        assert_eq!(p["b"], "x y");
        assert!(parse_params(b"a=1&a=2").is_err());
    }

    #[test]
    fn scopes() {
        assert_eq!(parse_scopes(None).unwrap(), vec!["mcp"]);
        assert_eq!(parse_scopes(Some("mcp mcp")).unwrap(), vec!["mcp"]);
        assert!(parse_scopes(Some("mcp admin")).is_err());
    }

    #[test]
    fn challenges() {
        assert!(valid_code_challenge(
            &sourcegull_crypto::pkce::challenge_s256("v")
        ));
        assert!(!valid_code_challenge("short"));
        assert!(!valid_code_challenge(&"a+".repeat(22)[..43]));
    }

    #[test]
    fn appends_params() {
        assert_eq!(
            with_params(
                "http://127.0.0.1:3000/cb?x=1",
                &[("code", "a b"), ("state", "s")]
            ),
            "http://127.0.0.1:3000/cb?x=1&code=a+b&state=s"
        );
        let page = error_page("No & way");
        assert_eq!(
            page.headers()[header::LOCATION],
            "/oauth/consent?error=No+%26+way"
        );
    }
}
