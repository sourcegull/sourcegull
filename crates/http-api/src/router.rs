//! Route sets and the shared middleware stack.

use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use sourcegull_services::AppState;
use sourcegull_types::metric_names;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::{
    assets, batch_files, compute, executors, health, repo_content, scip_upload, search, webhooks,
};

/// Default maximum request body size for extractors (`Json`, `Bytes`, `String`, ...).
/// A route that accepts larger uploads overrides it with its own [`DefaultBodyLimit`] layer.
pub const DEFAULT_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// Header carrying the request id (generated when the client does not send one).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Requests served, by method, route and status.
pub const HTTP_REQUESTS_METRIC: &str = metric_names::HTTP_REQUESTS_TOTAL;
/// Request latency in seconds, by method and route.
pub const HTTP_DURATION_METRIC: &str = metric_names::HTTP_REQUEST_DURATION_SECONDS;

/// Routes of the public listener (default `:7080`): `/healthz`, `/readyz`,
/// `/.api/site-config-schema`, bulk change spec workspace files (`/.api/files/batch-changes/…`), the
/// code host webhooks (`/.api/gitlab-webhooks`,
/// `/.api/webhooks/{uuid}`), streaming blame (`/.api/blame/…`), streaming search
/// (`/.api/search/stream`), SCIP uploads, the executor API (`/.executors/*`), and as the
/// fallback raw
/// repository files (`/{repo}[@{rev}]/-/raw/{path}`) followed by the embedded UI (SPA routing
/// for every other path, repository pages included; JSON 404 for unknown `/.api/*` and `/-/*`
/// paths).
///
/// Other crates add their routes with `.merge()` before calling [`finish`]:
///
/// ```text
/// let app = finish(public_routes().merge(graphql::routes()).merge(authn::routes()), state);
/// ```
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/.api/site-config-schema", get(health::site_config_schema))
        .route("/.api/settings-schema", get(health::settings_schema))
        .route("/.api/resources-schema", get(health::resources_schema))
        .route("/.api/gitlab-webhooks", post(webhooks::gitlab_webhook))
        .route("/.api/webhooks/{uuid}", post(webhooks::uuid_webhook))
        .route("/.api/blame/{*rest}", get(repo_content::blame_stream))
        .route("/.api/search/stream", get(search::stream))
        .route("/.api/compute/stream", get(compute::stream))
        .route("/.api/scip/upload", post(scip_upload::upload))
        .route("/.api/lsif/upload", post(scip_upload::lsif_gone))
        .route(
            "/.api/files/batch-changes/{spec}",
            post(batch_files::upload).layer(DefaultBodyLimit::max(batch_files::UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/.api/files/batch-changes/{spec}/{*file}",
            get(batch_files::download),
        )
        .route(
            "/.executors/files/batch-changes/{spec}/{*file}",
            get(batch_files::executor_download),
        )
        .route("/.executors/queue/heartbeat", post(executors::heartbeat))
        .route(
            sourcegull_executor_api::BATCHES_ADD_CACHE_ENTRY,
            post(executors::batches_add_cache_entry)
                .layer(DefaultBodyLimit::max(executors::BATCHES_UPLOAD_LIMIT)),
        )
        .route(
            sourcegull_executor_api::BATCHES_UPLOAD_RESULT,
            post(executors::batches_upload_result)
                .layer(DefaultBodyLimit::max(executors::BATCHES_UPLOAD_LIMIT)),
        )
        .route(
            "/.executors/queue/{queue}/{action}",
            post(executors::queue_action),
        )
        .route(
            "/.executors/git/{*rest}",
            get(executors::git_get).post(executors::git_post),
        )
        .merge(crate::scim::routes())
        .fallback(repo_content::fallback)
}

/// Routes of the internal listener (default `:6060`): `/metrics`, `/healthz`, `/readyz`.
pub fn internal_routes() -> Router<AppState> {
    Router::new()
        .route("/metrics", get(health::metrics))
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
}

/// The public router with middleware and state applied.
pub fn public_router(state: AppState) -> Router {
    finish(public_routes(), state)
}

/// The internal router with middleware and state applied.
pub fn internal_router(state: AppState) -> Router {
    finish(internal_routes(), state)
}

/// Applies the shared middleware stack to `routes` and binds `state`.
///
/// Outermost first: request id (`x-request-id`, generated if absent, echoed in the response),
/// a tracing span per request, security headers, gzip compression (skipped for small bodies,
/// images, gRPC and `text/event-stream`), request metrics, the default body limit, and
/// authentication with the CSRF check ([`authenticate`](crate::authenticate)).
pub fn finish(routes: Router<AppState>, state: AppState) -> Router {
    let request_id = HeaderName::from_static(REQUEST_ID_HEADER);
    let csp = HeaderValue::from_static(assets::content_security_policy());
    let stack = ServiceBuilder::new()
        .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))
        .layer(TraceLayer::new_for_http().make_span_with(make_span))
        .layer(PropagateRequestIdLayer::new(request_id))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            csp,
        ))
        // The default predicate already excludes text/event-stream, images, gRPC and bodies
        // under 32 bytes.
        .layer(CompressionLayer::new().gzip(true))
        .layer(middleware::from_fn(track_metrics))
        .layer(DefaultBodyLimit::max(DEFAULT_BODY_LIMIT));
    routes
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::authenticate,
        ))
        .layer(stack)
        .with_state(state)
}

fn make_span(req: &Request) -> tracing::Span {
    let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    tracing::info_span!(
        "http_request",
        method = %req.method(),
        path = %req.uri().path(),
        request_id,
    )
}

/// Records RED metrics labelled by the matched route pattern (not the raw path, to keep
/// cardinality bounded); requests handled by the fallback are labelled `fallback`.
async fn track_metrics(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "fallback".to_owned());
    let method = req.method().as_str().to_owned();
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    metrics::histogram!(HTTP_DURATION_METRIC, "method" => method.clone(), "route" => route.clone())
        .record(start.elapsed().as_secs_f64());
    metrics::counter!(HTTP_REQUESTS_METRIC, "method" => method, "route" => route, "status" => status)
        .increment(1);
    resp
}
