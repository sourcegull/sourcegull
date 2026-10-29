//! Built-in sign-up, sign-in and sign-out endpoints (`/-/…`).
//!
//! Request and response bodies are JSON: `{"ok": true}` on success, `{"error", "code"}` with a
//! 4xx/5xx status otherwise. Sign-up and sign-in set the [`SESSION_COOKIE`] (`HttpOnly`,
//! `SameSite=Lax`, `Path=/`, `Secure` when `externalURL` is `https`, `Max-Age` = the session
//! lifetime); sign-out clears it.
//!
//! Sign-in, password reset requests and access requests are limited per client address and
//! per account or email (`auth.rateLimits`): over a limit they answer 429 with `Retry-After`.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sourcegull_authn::AuthError;
use sourcegull_http_extract::{
    ClientIp, MaybeActor, RequestAuth, SESSION_COOKIE, auth_error_response, error_response,
    too_many_requests,
};
use sourcegull_services::AppState;
use sourcegull_services::rate_limits::{self, RateLimit};
use sourcegull_services::relay::marshal_id;

/// The `/-/sign-up`, `/-/sign-in`, `/-/sign-out` and `/-/auth-state` routes, plus GitLab OAuth and
/// OpenID Connect sign-in (`/.auth/*`). Merge into [`public_routes`](crate::public_routes) before
/// [`finish`](crate::finish).
pub fn auth_routes() -> Router<AppState> {
    crate::oauth_routes::oauth_routes()
        .merge(crate::oidc_routes::oidc_routes())
        .route("/-/sign-up", post(sign_up))
        .route("/-/sign-in", post(sign_in))
        .route("/-/sign-out", post(sign_out))
        .route("/-/auth-state", get(auth_state))
        .route("/-/reset-password-init", post(reset_password_init))
        .route("/-/reset-password-code", post(reset_password_code))
        .route("/-/verify-email", get(verify_email))
        .route("/.api/request-access", post(request_access))
        .route("/.api/set-invite-password", post(set_invite_password))
}

#[derive(Debug, Deserialize)]
struct SignUpBody {
    username: String,
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SignInBody {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct ResetInitBody {
    email: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetCodeBody {
    #[serde(rename = "userID")]
    user_id: String,
    code: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct VerifyEmailQuery {
    code: String,
    email: String,
}

/// `POST /-/reset-password-init {email}`: emails a reset link when the address is a verified
/// email of an account. The answer is the same either way and does not wait for the lookup or
/// the email: both run in a background task.
async fn reset_password_init(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: ResetInitBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let email = body.email.trim().to_owned();
    for (limit, subject) in [
        (RateLimit::PasswordResetPerAddress, rate_limits::address(ip)),
        (
            RateLimit::PasswordResetPerEmail,
            rate_limits::account(&email),
        ),
    ] {
        if let Err(l) = state.rate_limit_hit(limit, &subject).await {
            return too_many_requests(l.retry_after);
        }
    }
    if state.config.load().value.email_smtp.is_some() {
        tokio::spawn(send_password_reset(state, email));
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Issues and emails a reset link if `email` is a verified email of an account.
async fn send_password_reset(state: AppState, email: String) {
    match state.db.user_emails().user_by_verified_email(&email).await {
        Ok(Some(user)) => {
            if let Err(e) =
                sourcegull_services::email::issue_password_reset(&state, user, Some(&email)).await
            {
                tracing::error!(error = %e, "creating password reset failed");
            }
        }
        Err(e) => tracing::error!(error = %e, "looking up reset email failed"),
        Ok(None) => {}
    }
}

async fn reset_password_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: ResetCodeBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if let Err(e) = state.auth.check_password_policy(&body.password) {
        return auth_error_response(&e);
    }
    let Some(id) = sourcegull_services::relay::unmarshal_int_id("User", &body.user_id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "Invalid reset link.",
        );
    };
    let user = sourcegull_types::UserId(id);
    match state
        .db
        .password_resets()
        .redeem(
            user,
            &sourcegull_crypto::session::hash_session_id(&body.code),
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_INPUT",
                "The link is invalid, expired or already used.",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "redeeming password reset failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "Internal error.",
            );
        }
    }
    if let Err(e) = state.auth.set_password(user, &body.password).await {
        return auth_error_response(&e);
    }
    if let Err(e) = state.auth.end_user_credentials(user).await {
        return auth_error_response(&e);
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `GET /-/verify-email?code=…&email=…`: the link sent to an unverified email. Redirects to the
/// app, or to the sign-in page with the error (people open it from their mail client).
async fn verify_email(
    State(state): State<AppState>,
    Query(query): Query<VerifyEmailQuery>,
) -> Response {
    match state
        .db
        .user_emails()
        .redeem_verification_code(
            &query.email,
            &sourcegull_services::email::code_hash(&query.code),
        )
        .await
    {
        Ok(true) => crate::oauth_routes::redirect("/user/settings/emails?verified=1", &[]),
        Ok(false) => crate::oauth_routes::fail(
            &state,
            "The verification link is invalid or was already used.",
        ),
        Err(e) => {
            tracing::error!(error = %e, "verifying email failed");
            crate::oauth_routes::fail(&state, "Verifying the email failed. Try again.")
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessRequestBody {
    name: String,
    email: String,
    #[serde(default)]
    additional_info: String,
}

/// `POST /.api/request-access`: a visitor applies for an account (`auth.accessRequest`,
/// on by default when built-in sign-up is closed). The answer is the same whether or not the
/// email is already known.
async fn request_access(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !access_requests_enabled(&state) {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "Access requests are disabled.",
        );
    }
    let body: AccessRequestBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let (name, email, info) = (
        body.name.trim(),
        body.email.trim(),
        body.additional_info.trim(),
    );
    if name.is_empty() || name.len() > 255 || info.len() > 4096 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "Enter a name (at most 255 characters) and at most 4096 characters of details.",
        );
    }
    if let Err(e) = sourcegull_authn::validate::validate_email(email) {
        return auth_error_response(&e);
    }
    for (limit, subject) in [
        (RateLimit::AccessRequestPerAddress, rate_limits::address(ip)),
        (
            RateLimit::AccessRequestPerEmail,
            rate_limits::account(email),
        ),
    ] {
        if let Err(l) = state.rate_limit_hit(limit, &subject).await {
            return too_many_requests(l.retry_after);
        }
    }
    if let Err(e) = state.db.access_requests().create(name, email, info).await {
        tracing::error!(error = %e, "saving access request failed");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "Internal error.",
        );
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Whether visitors may submit access requests.
fn access_requests_enabled(state: &AppState) -> bool {
    state
        .config
        .load()
        .value
        .auth_access_request
        .enabled
        .unwrap_or(!state.auth.signup_allowed())
}

#[derive(Debug, Deserialize)]
struct SetInvitePasswordBody {
    token: String,
    password: String,
}

/// `POST /.api/set-invite-password`: uses up an approved access request's account setup link.
async fn set_invite_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: SetInvitePasswordBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    // Check the password first, so that a rejected one does not use up the link.
    if let Err(e) = state.auth.check_password_policy(&body.password) {
        return auth_error_response(&e);
    }
    let hash = sourcegull_crypto::session::hash_session_id(&body.token);
    let user = match state.db.access_requests().redeem_invite(&hash).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "INVALID_INPUT",
                "The link is invalid, expired or already used.",
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "redeeming invite failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                "Internal error.",
            );
        }
    };
    if let Err(e) = state.auth.set_password(user, &body.password).await {
        return auth_error_response(&e);
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Parses a JSON body, answering in the endpoints' JSON error format. Requires a JSON content
/// type, which a cross-site HTML form cannot send without a CORS preflight.
fn parse_json<T: DeserializeOwned>(headers: &HeaderMap, body: &Bytes) -> Result<T, Box<Response>> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Err(Box::new(error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "INVALID_INPUT",
            "Expected a JSON request body (Content-Type: application/json).",
        )));
    }
    serde_json::from_slice(body).map_err(|e| {
        Box::new(error_response(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            &format!("Invalid request body: {e}"),
        ))
    })
}

fn ok_with_cookie(cookie: HeaderValue) -> Response {
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    let h = resp.headers_mut();
    h.insert(header::SET_COOKIE, cookie);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Whether `externalURL` is `https` (cookies then get `Secure`).
pub(crate) fn external_url_is_https(state: &AppState) -> bool {
    state
        .config
        .load()
        .value
        .external_url
        .as_deref()
        .is_some_and(|u| u.len() >= 8 && u[..8].eq_ignore_ascii_case("https://"))
}

/// Builds the `Set-Cookie` value for a session token (or, with `None`, one that clears it).
pub(crate) fn session_set_cookie(
    state: &AppState,
    token: Option<&str>,
    ttl: Duration,
) -> HeaderValue {
    let secure = external_url_is_https(state);
    let (value, max_age) = match token {
        Some(t) => (t, ttl.as_secs()),
        None => ("", 0),
    };
    let mut cookie =
        format!("{SESSION_COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    // Session tokens are URL-safe base64; the value can only fail to be a header if a session
    // store ever produced something else.
    HeaderValue::from_str(&cookie)
        .unwrap_or_else(|_| HeaderValue::from_static("sourcegull_session="))
}

/// Queues a permissions sync of a user who just signed in. Failures are logged: the
/// scheduler syncs every user eventually.
pub(crate) async fn queue_perms_sync(state: &AppState, user: sourcegull_types::UserId) {
    if let Err(e) = state
        .db
        .perms_sync_jobs()
        .enqueue(
            sourcegull_db::PermsSyncSubject::User(user),
            sourcegull_db::PermsSyncReason::UserSignedIn,
            None,
        )
        .await
    {
        tracing::warn!(user = user.get(), error = %e, "queueing a permissions sync failed");
    }
}

/// Drops the session the request came with, if any (a new one is about to replace it).
pub(crate) async fn end_previous_session(state: &AppState, auth: Option<&RequestAuth>) {
    if let Some(old) = auth.and_then(|a| a.session.as_ref())
        && let Err(e) = state.auth.sessions().delete(old).await
    {
        tracing::warn!(error = %e, "deleting the previous session");
    }
}

/// `POST /-/sign-up {username, email, password}`: creates a built-in account and signs it in.
/// The first user becomes site admin.
async fn sign_up(
    State(state): State<AppState>,
    auth: Option<Extension<RequestAuth>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: SignUpBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let user = match state
        .auth
        .sign_up(&body.username, &body.email, &body.password)
        .await
    {
        Ok(u) => u,
        Err(e) => return auth_error_response(&e),
    };
    if let Err(e) =
        sourcegull_services::email::send_verification(&state, user.id, &body.email).await
    {
        tracing::warn!(error = %e, "creating verification link failed");
    }
    let token = match state.auth.start_session(user.id).await {
        Ok(t) => t,
        Err(e) => return auth_error_response(&e),
    };
    end_previous_session(&state, auth.as_deref()).await;
    queue_perms_sync(&state, user.id).await;
    ok_with_cookie(session_set_cookie(
        &state,
        Some(token.as_str()),
        state.auth.session_ttl(),
    ))
}

/// `POST /-/sign-in {username, password}`: the username may also be a verified email. Limited
/// per client address (every attempt) and per account (failed attempts, from any address).
async fn sign_in(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    auth: Option<Extension<RequestAuth>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: SignInBody = match parse_json(&headers, &body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let account = rate_limits::account(&body.username);
    if let Err(l) = state
        .rate_limit_hit(RateLimit::SignInPerAddress, &rate_limits::address(ip))
        .await
    {
        return too_many_requests(l.retry_after);
    }
    if let Err(l) = state
        .rate_limit_check(RateLimit::FailedSignInsPerAccount, &account)
        .await
    {
        return too_many_requests(l.retry_after);
    }
    let (user, token) = match state.auth.sign_in(&body.username, &body.password, ip).await {
        Ok(r) => r,
        Err(e) => {
            if matches!(e, AuthError::InvalidCredentials) {
                // Counts toward the account's limit on failures.
                let _ = state
                    .rate_limit_hit(RateLimit::FailedSignInsPerAccount, &account)
                    .await;
            }
            return auth_error_response(&e);
        }
    };
    end_previous_session(&state, auth.as_deref()).await;
    queue_perms_sync(&state, user.id).await;
    ok_with_cookie(session_set_cookie(
        &state,
        Some(token.as_str()),
        state.auth.session_ttl(),
    ))
}

/// `POST /-/sign-out`: ends the cookie's session and clears the cookie.
async fn sign_out(State(state): State<AppState>, auth: Option<Extension<RequestAuth>>) -> Response {
    if let Some(token) = auth.as_deref().and_then(|a| a.session.as_ref())
        && let Err(e) = state.auth.sign_out(token).await
    {
        return auth_error_response(&e);
    }
    ok_with_cookie(session_set_cookie(&state, None, Duration::ZERO))
}

/// `GET /-/auth-state`: `{authenticated, user?: {id, username, siteAdmin}, allowSignup,
/// needsSiteInit, builtin, providers}`. `builtin` says whether password sign-in is enabled;
/// `providers` lists the external sign-in buttons (`{type, displayName, authenticationURL}`).
async fn auth_state(State(state): State<AppState>, MaybeActor(actor): MaybeActor) -> Response {
    let needs_site_init = match state.auth.needs_site_init().await {
        Ok(v) => v,
        Err(e) => return auth_error_response(&e),
    };
    let mut body = serde_json::json!({
        "authenticated": actor.is_some(),
        "allowSignup": state.auth.signup_allowed(),
        "accessRequest": access_requests_enabled(&state),
        "needsSiteInit": needs_site_init,
        "builtin": state.auth.builtin_enabled(),
        "providers": crate::oauth_routes::provider_buttons(&state),
    });
    if let Some(a) = &actor {
        body["user"] = serde_json::json!({
            "id": marshal_id("User", a.user_id.get()),
            "username": a.username,
            "siteAdmin": a.site_admin,
        });
        // Whether the viewer may use bulk changes (`batchChangesEnabled`, with
        // `restrictToAdmins` applied): the UI shows the nav entry from it.
        body["batchChangesEnabled"] = serde_json::json!(
            sourcegull_services::batches::access::available(&state.config.load().value, a).is_ok()
        );
    }
    let mut resp = Json(body).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
