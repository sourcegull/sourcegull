//! Request authentication and CSRF protection.
//!
//! [`authenticate`] runs on every request (it is part of [`finish`](crate::finish)). It resolves
//! the caller from `Authorization: token …` / `token-sudo …` or, failing that, the
//! [`SESSION_COOKIE`], and stores a [`RequestAuth`] in the request extensions. Handlers read it
//! with the extractors of `sourcegull-http-extract` (`MaybeActor`, `RequireActor`).
//!
//! CSRF rule: a request authenticated by the session cookie whose method is not `GET`/`HEAD`
//! must carry `X-Requested-With: Sourcegull`, otherwise it is rejected with 403. Browsers only let
//! same-origin scripts set that header (cross-origin it needs a CORS preflight, which we never
//! grant). Token-authenticated requests are exempt: a browser never attaches tokens on its own.
//!
//! When the TCP peer is a trusted proxy (`auth.allowedIpAddress.trustedClientIpAddress`, used
//! whether or not the allowlist is enabled), the client address it reports in
//! `clientIpRequestHeader` is stored as a [`ForwardedClientIp`]; the `ClientIp` extractor, and
//! with it the sign-in rate limits and lockout, use that address.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use ipnet::IpNet;
use sourcegull_authn::{AuthError, AuthVia, ExternalIdentity, SessionToken};
use sourcegull_config::AuthProvider;
use sourcegull_http_extract::{
    CSRF_CODE, ForwardedClientIp, REQUESTED_WITH_HEADER, REQUESTED_WITH_VALUE, RequestAuth,
    SESSION_COOKIE, auth_error_response, error_response,
};
use sourcegull_services::AppState;

/// The value of the [`SESSION_COOKIE`] in a request's `Cookie` headers, if present.
pub fn session_cookie(headers: &HeaderMap) -> Option<SessionToken> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, value)| *name == SESSION_COOKIE && !value.is_empty())
        .map(|(_, value)| SessionToken::new(value.trim_matches('"')))
}

/// Middleware resolving the caller (see the module docs). Invalid credentials in an
/// `Authorization` header fail the request with 401; an unknown or expired session cookie is
/// treated as anonymous.
pub async fn authenticate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let policy = &state.config.load().value.auth_allowed_ip_address;
    if policy.enabled && !matches!(req.uri().path(), "/healthz" | "/readyz" | "/metrics") {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        let client = peer.and_then(|peer| client_ip(peer, req.headers(), policy));
        if !client.is_some_and(|ip| in_networks(ip, &policy.user_ip_address)) {
            return error_response(
                StatusCode::FORBIDDEN,
                "FORBIDDEN",
                "IP address is not allowed.",
            );
        }
    }
    let mut auth = RequestAuth::default();
    if let Some(value) = req.headers().get(header::AUTHORIZATION) {
        let Ok(value) = value.to_str() else {
            return auth_error_response(&AuthError::TokenInvalid);
        };
        match state.auth.authenticate_access_token(value).await {
            Ok(actor) => auth.actor = actor,
            Err(e) => return auth_error_response(&e),
        }
    }
    auth.session = session_cookie(req.headers());
    let mut via_cookie = false;
    if auth.actor.is_none()
        && let Some(token) = &auth.session
    {
        match state.auth.authenticate_session(token).await {
            Ok(actor) => {
                via_cookie = actor.is_some();
                auth.actor = actor;
            }
            Err(e) => return auth_error_response(&e),
        }
    }
    if auth.actor.is_none() {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        for provider in &state.config.load().value.auth_providers {
            let AuthProvider::HttpHeader(provider) = provider else {
                continue;
            };
            let trusted = peer.is_some_and(|ip| {
                provider
                    .trusted_proxies
                    .iter()
                    .any(|cidr| cidr.parse::<IpNet>().is_ok_and(|net| net.contains(&ip)))
            });
            if !trusted {
                continue;
            }
            let values = req.headers().get_all(provider.username_header.as_str());
            let mut values = values.iter();
            let Some(raw) = values.next().and_then(|v| v.to_str().ok()) else {
                continue;
            };
            if values.next().is_some() {
                continue;
            }
            let raw = provider
                .strip_username_header_prefix
                .as_deref()
                .and_then(|prefix| raw.strip_prefix(prefix))
                .unwrap_or(raw)
                .trim();
            let username = normalize_username(raw);
            if username.is_empty() {
                continue;
            }
            let email = provider
                .email_header
                .as_ref()
                .and_then(|h| req.headers().get(h.as_str()))
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            let identity = ExternalIdentity {
                service_type: "http-header".into(),
                service_id: "trusted-proxy".into(),
                client_id: String::new(),
                account_id: raw.to_owned(),
                username,
                display_name: None,
                avatar_url: None,
                email,
                email_verified: true,
                account_data: serde_json::json!({"username": raw}),
                auth_data: None,
            };
            let accounts = state.db.external_accounts(state.keyring.clone());
            let signed = match state
                .auth
                .authenticate_header_identity(&accounts, &identity)
                .await
            {
                Ok(signed) => signed,
                Err(e) => return auth_error_response(&e),
            };
            match state.auth.actor_for(signed.user.id, AuthVia::Session).await {
                Ok(actor) => {
                    via_cookie = actor.is_some();
                    auth.actor = actor;
                }
                Err(e) => return auth_error_response(&e),
            }
            break;
        }
    }
    if via_cookie && !matches!(*req.method(), Method::GET | Method::HEAD) {
        let ok = req
            .headers()
            .get(REQUESTED_WITH_HEADER)
            .is_some_and(|v| v.as_bytes() == REQUESTED_WITH_VALUE.as_bytes());
        if !ok {
            return error_response(
                StatusCode::FORBIDDEN,
                CSRF_CODE,
                "Cookie-authenticated requests that change state must send the header \
                 X-Requested-With: Sourcegull.",
            );
        }
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    if let Some(peer) = peer
        && let Some(client) = client_ip(peer, req.headers(), policy)
        && client != peer
    {
        req.extensions_mut().insert(ForwardedClientIp(client));
    }
    req.extensions_mut().insert(auth);
    next.run(req).await
}

fn in_networks(ip: IpAddr, networks: &[String]) -> bool {
    networks.iter().any(|entry| {
        entry.parse::<IpNet>().is_ok_and(|net| net.contains(&ip))
            || entry.parse::<IpAddr>().is_ok_and(|addr| addr == ip)
    })
}

fn client_ip(
    peer: IpAddr,
    headers: &HeaderMap,
    policy: &sourcegull_config::AllowedIpAddress,
) -> Option<IpAddr> {
    if !in_networks(peer, &policy.trusted_client_ip_address) {
        return Some(peer);
    }
    let Ok(header) = axum::http::HeaderName::from_bytes(policy.client_ip_request_header.as_bytes())
    else {
        return None;
    };
    let mut values = headers.get_all(header).iter();
    let Some(value) = values.next().and_then(|v| v.to_str().ok()) else {
        return Some(peer);
    };
    if values.next().is_some() {
        return None;
    }
    // Walk from the trusted TCP peer toward the client. A proxy cannot claim that an
    // untrusted hop farther left was the client.
    let mut client = peer;
    for hop in value.split(',').rev() {
        if !in_networks(client, &policy.trusted_client_ip_address) {
            break;
        }
        let Ok(ip) = hop.trim().parse::<IpAddr>() else {
            return None;
        };
        client = ip;
    }
    Some(client)
}

fn normalize_username(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars().take(255) {
        if c.is_ascii_alphanumeric()
            || c == '_'
            || (matches!(c, '-' | '.') && !out.is_empty() && !out.ends_with(['-', '.']))
        {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches(['-', '.']).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn parses_session_cookie() {
        let mut h = HeaderMap::new();
        assert!(session_cookie(&h).is_none());
        h.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; sourcegull_session=abc"),
        );
        assert_eq!(
            session_cookie(&h).map(|t| t.into_string()),
            Some("abc".into())
        );
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            HeaderValue::from_static("sourcegull_session="),
        );
        h.append(
            header::COOKIE,
            HeaderValue::from_static("x=y;sourcegull_session=\"q\""),
        );
        assert_eq!(
            session_cookie(&h).map(|t| t.into_string()),
            Some("q".into())
        );
    }
}
