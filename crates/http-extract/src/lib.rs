//! sourcegull-http-extract: what handlers of every crate that adds routes share.
//!
//! The authentication middleware (`sourcegull-http-api`) stores a [`RequestAuth`] in the request
//! extensions; handlers read the caller with the [`MaybeActor`] / [`RequireActor`]
//! extractors and the TCP peer with [`ClientIp`]. [`error_response`] and
//! [`auth_error_response`] are the JSON error bodies of every API. The session cookie and the
//! CSRF header are named here too.

use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sourcegull_authn::{AccessError, Actor, AuthError, SessionToken};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "sourcegull_session";
/// Header required on cookie-authenticated non-GET/HEAD requests.
pub const REQUESTED_WITH_HEADER: &str = "x-requested-with";
/// The required value of [`REQUESTED_WITH_HEADER`].
pub const REQUESTED_WITH_VALUE: &str = "Sourcegull";
/// Error code of a request rejected by the CSRF rule.
pub const CSRF_CODE: &str = "CSRF";

/// Who made the request, as resolved by the authentication middleware.
#[derive(Debug, Clone, Default)]
pub struct RequestAuth {
    /// The authenticated user, if any.
    pub actor: Option<Actor>,
    /// The session cookie the request carried, valid or not (used by sign-out).
    pub session: Option<SessionToken>,
}

/// Extractor for the optional caller. Never rejects.
#[derive(Debug, Clone)]
pub struct MaybeActor(pub Option<Actor>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeActor {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<RequestAuth>()
                .and_then(|a| a.actor.clone()),
        ))
    }
}

/// Extractor for a signed-in caller; rejects anonymous requests with 401.
#[derive(Debug, Clone)]
pub struct RequireActor(pub Actor);

impl<S: Send + Sync> FromRequestParts<S> for RequireActor {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestAuth>()
            .and_then(|a| a.actor.clone())
            .map(Self)
            .ok_or_else(|| {
                error_response(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHORIZED",
                    "You must be signed in.",
                )
            })
    }
}

/// Extractor for the client's IP address: the [`ForwardedClientIp`] when a trusted proxy named
/// it, else the TCP peer's. `None` when the router runs without connect info.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

/// The client address a trusted proxy reported (`auth.allowedIpAddress`), stored in the request
/// extensions by the authentication middleware when it differs from the TCP peer.
#[derive(Debug, Clone, Copy)]
pub struct ForwardedClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        if let Some(forwarded) = parts.extensions.get::<ForwardedClientIp>() {
            return Ok(Self(Some(forwarded.0)));
        }
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip()),
        ))
    }
}

/// A JSON error response `{"error": message, "code": code}`.
pub fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let mut resp = (
        status,
        Json(serde_json::json!({ "error": message, "code": code })),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// A 429 JSON error response (`RATE_LIMITED`) with `Retry-After` in whole seconds.
pub fn too_many_requests(retry_after: std::time::Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    let mut resp = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "RATE_LIMITED",
        &format!("Too many requests. Try again in {secs} seconds."),
    );
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs));
    resp
}

/// The JSON error response for an [`AuthError`], with its suggested status. Internal errors are
/// logged; their details never reach the client.
pub fn auth_error_response(err: &AuthError) -> Response {
    if let AuthError::Internal(source) = err {
        tracing::error!(error = %source, "authentication failed internally");
    }
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(status, err.code(), &err.to_string())
}

/// The JSON error response for an [`AccessError`] from a feature service, with its suggested
/// status. Internal errors are logged; their details never reach the client.
pub fn access_error_response(err: &AccessError) -> Response {
    if let AccessError::Internal(source) = err {
        tracing::error!(error = %source, "checking access failed internally");
    }
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(status, err.code(), &err.to_string())
}
