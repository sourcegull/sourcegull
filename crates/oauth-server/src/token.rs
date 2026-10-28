//! The token endpoint (`authorization_code` and `refresh_token` grants) and token revocation
//! (RFC 7009).
//!
//! Requests are `application/x-www-form-urlencoded`. Clients are public: they identify with
//! `client_id` (in the form, or as the user name of HTTP Basic authentication, whose password
//! is ignored) and prove the authorization code is theirs with the PKCE `code_verifier`.
//!
//! Access tokens last `mcp.oauth.accessTokenTTL` (1 hour) and refresh tokens
//! `mcp.oauth.refreshTokenTTL` (30 days). Each refresh returns a new refresh token; presenting
//! an exchanged one again means it was copied, so the whole grant is revoked. Likewise an
//! authorization code presented again revokes the grant created from it (RFC 6749 §4.1.2).

#![allow(clippy::result_large_err)]

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use serde_json::json;
use sourcegull_authn::events;
use sourcegull_authn::oauth::{
    canonical_resource, generate_access_token, generate_refresh_token, refresh_token_hash,
    token_hash,
};
use sourcegull_crypto::constant_time_eq;
use sourcegull_crypto::pkce::challenge_s256;
use sourcegull_crypto::session::hash_session_id;
use sourcegull_db::{NewOAuthToken, OAuthClient, RefreshOutcome};
use sourcegull_services::AppState;

use crate::authorize::{parse_params, parse_scopes};
use crate::clients::{self, ClientError};
use crate::{not_found, oauth_error, public_json, public_json_status, resource_url, server_error};

fn invalid_request(description: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_request", description)
}

fn invalid_grant(description: &str) -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", description)
}

fn invalid_client(description: &str) -> Response {
    oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", description)
}

/// The `client_id` of a request: the form's, or the user name of HTTP Basic authentication.
fn client_id(params: &HashMap<String, String>, headers: &HeaderMap) -> Result<String, Response> {
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("basic").then_some(rest.trim())
        })
        .and_then(|b64| STANDARD.decode(b64).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|s| {
            let id = s.split_once(':').map_or(s.as_str(), |(id, _)| id);
            url::form_urlencoded::parse(format!("x={id}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
        })
        .filter(|id| !id.is_empty());
    let form = params.get("client_id").filter(|v| !v.is_empty());
    match (form, basic) {
        (Some(a), Some(b)) if *a != b => Err(invalid_request(
            "client_id differs between the form and the Authorization header.",
        )),
        (Some(a), _) => Ok(a.clone()),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(invalid_client("The request has no client_id.")),
    }
}

async fn client(state: &AppState, id: &str) -> Result<OAuthClient, Response> {
    match clients::stored(state, id).await {
        Ok(c) => Ok(c),
        Err(ClientError::Invalid(m)) => Err(invalid_client(&m)),
        Err(ClientError::SignInRequired) => Err(invalid_client("Unknown client.")),
        Err(ClientError::Internal(e)) => Err(server_error("resolving an OAuth client", e)),
    }
}

/// The parameters of a token request; empty values count as absent.
struct Params(HashMap<String, String>);

impl Params {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }
}

/// New access and refresh tokens with their expiry from the site configuration.
struct Issued {
    access: (String, [u8; 32]),
    refresh: (String, [u8; 32]),
    access_ttl: std::time::Duration,
    access_expires: chrono::DateTime<Utc>,
    refresh_expires: chrono::DateTime<Utc>,
}

impl Issued {
    fn new(state: &AppState) -> Self {
        let config = state.config.load();
        let access_ttl = config.value.mcp_oauth_access_token_ttl.get();
        let refresh_ttl = config.value.mcp_oauth_refresh_token_ttl.get();
        let now = Utc::now();
        let add = |d: std::time::Duration| {
            chrono::Duration::from_std(d)
                .ok()
                .and_then(|d| now.checked_add_signed(d))
                .unwrap_or(now)
        };
        Self {
            access: generate_access_token(),
            refresh: generate_refresh_token(),
            access_ttl,
            access_expires: add(access_ttl),
            refresh_expires: add(refresh_ttl),
        }
    }

    fn access(&self) -> NewOAuthToken<'_> {
        NewOAuthToken {
            hash: &self.access.1,
            expires_at: self.access_expires,
        }
    }

    fn refresh(&self) -> NewOAuthToken<'_> {
        NewOAuthToken {
            hash: &self.refresh.1,
            expires_at: self.refresh_expires,
        }
    }

    fn response(&self, scopes: &[String]) -> Response {
        public_json(json!({
            "access_token": self.access.0,
            "token_type": "Bearer",
            "expires_in": self.access_ttl.as_secs(),
            "refresh_token": self.refresh.0,
            "scope": scopes.join(" "),
        }))
    }
}

/// `POST /.oauth/token`.
pub async fn token(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !crate::enabled(&state) {
        return not_found();
    }
    let params = match parse_params(&body) {
        Ok(p) => p,
        Err(e) => return invalid_request(&e),
    };
    let client_id = match client_id(&params, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let client = match client(&state, &client_id).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let params = Params(params);
    match params.get("grant_type") {
        Some("authorization_code") => exchange_code(&state, &client, &params).await,
        Some("refresh_token") => {
            let resource = resource_url(&state, &headers);
            refresh(&state, &client, resource, &params).await
        }
        Some(other) => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            &format!("The grant type {other:?} is not supported."),
        ),
        None => invalid_request("The request has no grant_type."),
    }
}

async fn exchange_code(state: &AppState, client: &OAuthClient, params: &Params) -> Response {
    let Some(code) = params.get("code") else {
        return invalid_request("The request has no code.");
    };
    let Some(verifier) = params.get("code_verifier") else {
        return invalid_request("PKCE is required: send code_verifier.");
    };
    let store = state.db.oauth();
    let code_hash = hash_session_id(code);
    let authorization = match store.take_code(&code_hash).await {
        Ok(Some(a)) => a,
        Ok(None) => return replayed_code(state, client, &code_hash).await,
        Err(e) => return server_error("taking an OAuth authorization code", e),
    };
    if authorization.client_id != client.id {
        return invalid_grant("The authorization code was issued to another client.");
    }
    if params
        .get("redirect_uri")
        .is_some_and(|r| r != authorization.redirect_uri)
    {
        return invalid_grant("The redirect_uri differs from the authorization request's.");
    }
    if !(43..=128).contains(&verifier.len())
        || !constant_time_eq(
            challenge_s256(verifier).as_bytes(),
            authorization.code_challenge.as_bytes(),
        )
    {
        return invalid_grant("The code_verifier does not match the code_challenge.");
    }
    if let Some(r) = params.get("resource")
        && canonical_resource(r).as_deref() != Some(authorization.resource.as_str())
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "The resource differs from the authorization request's.",
        );
    }
    let Some(user) = authorization.user_id else {
        return invalid_grant(CODE_INVALID);
    };
    let issued = Issued::new(state);
    let grant = match store
        .create_grant(&authorization, user, issued.access(), issued.refresh())
        .await
    {
        Ok(g) => g,
        Err(e) if e.is_not_found() => return invalid_grant(CODE_INVALID),
        Err(e) => return server_error("creating an OAuth grant", e),
    };
    state
        .auth
        .log_event(
            events::OAUTH_GRANT_CREATED,
            Some(user),
            json!({ "grantId": grant, "clientId": client.client_id, "clientName": client.client_name }),
        )
        .await;
    issued.response(&authorization.scopes)
}

const CODE_INVALID: &str = "The authorization code is invalid, expired or used.";

/// Answers a code that is not live. A code used before was copied: the tokens issued for it
/// are revoked (RFC 6749 §4.1.2).
async fn replayed_code(state: &AppState, client: &OAuthClient, code_hash: &[u8; 32]) -> Response {
    let revoked = match state.db.oauth().revoke_replayed_code(code_hash).await {
        Ok(r) => r,
        Err(e) => return server_error("revoking the grant of a replayed OAuth code", e),
    };
    for (grant, user) in revoked {
        state
            .auth
            .log_event(
                events::OAUTH_CODE_REPLAYED,
                Some(user),
                json!({ "grantId": grant, "clientId": client.client_id }),
            )
            .await;
    }
    invalid_grant(CODE_INVALID)
}

async fn refresh(
    state: &AppState,
    client: &OAuthClient,
    resource: Option<String>,
    params: &Params,
) -> Response {
    let Some(token) = params.get("refresh_token") else {
        return invalid_request("The request has no refresh_token.");
    };
    let Some(hash) = refresh_token_hash(token) else {
        return invalid_grant("The refresh token is invalid.");
    };
    // Checked before the token is exchanged, so a bad request does not use it up. Grants have
    // the only scope and resource there are.
    if let Err(e) = parse_scopes(params.get("scope")) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_scope", &e);
    }
    if let Some(r) = params.get("resource")
        && canonical_resource(r) != resource
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "Tokens of this server are for its MCP endpoint.",
        );
    }
    let issued = Issued::new(state);
    let outcome = match state
        .db
        .oauth()
        .rotate_refresh_token(&hash, client.id, issued.access(), issued.refresh())
        .await
    {
        Ok(o) => o,
        Err(e) => return server_error("refreshing an OAuth token", e),
    };
    match outcome {
        RefreshOutcome::Rotated {
            grant_id,
            user_id,
            scopes,
            ..
        } => {
            state
                .auth
                .log_event(
                    events::OAUTH_TOKEN_REFRESHED,
                    Some(user_id),
                    json!({ "grantId": grant_id, "clientId": client.client_id }),
                )
                .await;
            issued.response(&scopes)
        }
        RefreshOutcome::Reused { grant_id, user_id } => {
            state
                .auth
                .log_event(
                    events::OAUTH_REFRESH_TOKEN_REUSED,
                    Some(user_id),
                    json!({ "grantId": grant_id, "clientId": client.client_id }),
                )
                .await;
            invalid_grant("The refresh token was already used; the grant is revoked.")
        }
        RefreshOutcome::Invalid => invalid_grant("The refresh token is invalid or expired."),
    }
}

/// `POST /.oauth/revoke`: revokes the grant of an access or refresh token. Answers 200 for
/// unknown tokens too, as RFC 7009 asks.
pub async fn revoke(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !crate::enabled(&state) {
        return not_found();
    }
    let params = match parse_params(&body) {
        Ok(p) => p,
        Err(e) => return invalid_request(&e),
    };
    let client_id = match client_id(&params, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let client = match client(&state, &client_id).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(token) = params.get("token").filter(|t| !t.is_empty()) else {
        return invalid_request("The request has no token.");
    };
    if let Some(hash) = token_hash(token) {
        match state.db.oauth().revoke_by_token(&hash, client.id).await {
            Ok(Some((grant, user))) => {
                state
                    .auth
                    .log_event(
                        events::OAUTH_GRANT_REVOKED,
                        Some(user),
                        json!({ "grantId": grant, "clientId": client.client_id, "by": "client" }),
                    )
                    .await;
            }
            Ok(None) => {}
            Err(e) => return server_error("revoking an OAuth token", e),
        }
    }
    public_json_status(StatusCode::OK, json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn client_id_from_form_or_basic() {
        let mut params = HashMap::new();
        let mut headers = HeaderMap::new();
        assert!(client_id(&params, &headers).is_err());
        params.insert("client_id".to_owned(), "sourcegull-a".to_owned());
        assert_eq!(client_id(&params, &headers).unwrap(), "sourcegull-a");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {}", STANDARD.encode("sourcegull-b:"))).unwrap(),
        );
        assert!(client_id(&params, &headers).is_err());
        params.clear();
        assert_eq!(client_id(&params, &headers).unwrap(), "sourcegull-b");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "Basic {}",
                STANDARD.encode("https%3A%2F%2Fa.example.com%2Fc.json:x")
            ))
            .unwrap(),
        );
        assert_eq!(
            client_id(&params, &headers).unwrap(),
            "https://a.example.com/c.json"
        );
    }
}
