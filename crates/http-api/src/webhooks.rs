//! Incoming code host webhooks: `POST /.api/gitlab-webhooks` and
//! `POST /.api/webhooks/{uuid}`.
//!
//! Both are unauthenticated (GitLab sends no session or token); deliveries are verified with
//! the connection's shared secret by [`sourcegull_repos::WebhookIngest`], which also applies and
//! logs them. `/.api/gitlab-webhooks` matches the secret against every GitLab connection,
//! optionally narrowed with `?externalServiceID=` (a number or an `ExternalService` node ID);
//! `/.api/webhooks/{uuid}` addresses one connection by its webhook UUID. Merge request and
//! pipeline events update changesets through [`AppState::changeset_hooks`].
//!
//! Rejected deliveries (4xx: unknown connection, wrong secret, bad payload) are limited per
//! client address (`auth.rateLimits.failedWebhooksPerAddress`); over the limit every delivery
//! from the address is refused with 429 and `Retry-After` until the window ends.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sourcegull_http_extract::{ClientIp, error_response, too_many_requests};
use sourcegull_repos::{Registry, WebhookIngest, WebhookOutcome, WebhookTarget};
use sourcegull_services::AppState;
use sourcegull_services::rate_limits::{self, RateLimit};
use sourcegull_services::relay::{EXTERNAL_SERVICE_KIND, unmarshal_int_id};
use sourcegull_types::ExternalServiceId;

#[derive(Debug, Deserialize)]
pub(crate) struct GitLabQuery {
    #[serde(rename = "externalServiceID")]
    external_service_id: Option<String>,
}

fn ingest(state: &AppState) -> WebhookIngest {
    WebhookIngest::new(state.db.clone(), state.keyring.clone(), Registry::default())
        .with_changeset_hooks(state.changeset_hooks.clone())
}

/// Fails with 429 when the address used up its rejected deliveries.
async fn check_failures(state: &AppState, address: &str) -> Result<(), Response> {
    state
        .rate_limit_check(RateLimit::FailedWebhooksPerAddress, address)
        .await
        .map_err(|l| too_many_requests(l.retry_after))
}

/// Answers `outcome`, counting a rejected delivery against the address.
async fn respond(state: &AppState, address: &str, outcome: WebhookOutcome) -> Response {
    if (400..500).contains(&outcome.status) {
        let _ = state
            .rate_limit_hit(RateLimit::FailedWebhooksPerAddress, address)
            .await;
    }
    response(outcome)
}

fn response(outcome: WebhookOutcome) -> Response {
    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if status.is_success() {
        (
            status,
            Json(serde_json::json!({ "status": outcome.message })),
        )
            .into_response()
    } else {
        let code = match status {
            StatusCode::UNAUTHORIZED => "UNAUTHORIZED",
            StatusCode::NOT_FOUND => "NOT_FOUND",
            StatusCode::BAD_REQUEST => "INVALID_INPUT",
            _ => "INTERNAL",
        };
        error_response(status, code, &outcome.message)
    }
}

/// `POST /.api/gitlab-webhooks`.
pub(crate) async fn gitlab_webhook(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(query): Query<GitLabQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let address = rate_limits::address(ip);
    if let Err(r) = check_failures(&state, &address).await {
        return r;
    }
    let external_service = match query.external_service_id.as_deref() {
        None | Some("") => None,
        Some(raw) => match raw
            .parse::<i64>()
            .ok()
            .or_else(|| unmarshal_int_id(EXTERNAL_SERVICE_KIND, raw))
        {
            Some(id) => Some(ExternalServiceId(id)),
            None => {
                let _ = state
                    .rate_limit_hit(RateLimit::FailedWebhooksPerAddress, &address)
                    .await;
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "INVALID_INPUT",
                    "invalid externalServiceID",
                );
            }
        },
    };
    let outcome = ingest(&state)
        .handle(WebhookTarget::GitLab { external_service }, &headers, &body)
        .await;
    respond(&state, &address, outcome).await
}

/// `POST /.api/webhooks/{uuid}`.
pub(crate) async fn uuid_webhook(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let address = rate_limits::address(ip);
    if let Err(r) = check_failures(&state, &address).await {
        return r;
    }
    let outcome = ingest(&state)
        .handle(WebhookTarget::Uuid(&uuid), &headers, &body)
        .await;
    respond(&state, &address, outcome).await
}
