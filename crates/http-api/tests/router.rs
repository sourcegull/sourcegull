//! Router tests driven with `tower::ServiceExt::oneshot`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_db::sqlx::postgres::PgPoolOptions;
use sourcegull_http_api::{CACHE_IMMUTABLE, CACHE_REVALIDATE, internal_router, public_router};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

fn metrics() -> MetricsHandle {
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    MetricsHandle::from_prometheus(recorder.handle())
}

/// State whose database is unreachable (nothing listens on port 1).
fn offline_state() -> AppState {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://sourcegull@127.0.0.1:1/sourcegull")
        .expect("lazy pool");
    state_for(Db::from_pool(pool))
}

fn state_for(db: Db) -> AppState {
    let config = SiteConfigHandle::default();
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    AppState::new(db, config, metrics(), auth)
}

async fn send(router: &Router, req: Request<Body>) -> axum::response::Response {
    router.clone().oneshot(req).await.expect("infallible")
}

async fn get(router: &Router, uri: &str) -> axum::response::Response {
    send(
        router,
        Request::get(uri).body(Body::empty()).expect("request"),
    )
    .await
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

#[tokio::test]
async fn healthz_is_always_ok() {
    let state = offline_state();
    for router in [public_router(state.clone()), internal_router(state)] {
        let resp = get(&router, "/healthz").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key("x-request-id"));
        assert_eq!(resp.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(resp.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(json(resp).await["status"], "ok");
    }
}

#[tokio::test]
async fn request_id_is_propagated() {
    let router = public_router(offline_state());
    let req = Request::get("/healthz")
        .header("x-request-id", "abc-123")
        .body(Body::empty())
        .expect("request");
    let resp = send(&router, req).await;
    assert_eq!(resp.headers()["x-request-id"], "abc-123");
}

#[tokio::test]
async fn readyz_reports_unreachable_database() {
    let state = offline_state();
    state.readiness.set_migrations_current(true);
    state.readiness.set_config_loaded(true);
    let resp = get(&public_router(state), "/readyz").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json(resp).await;
    assert_eq!(body["ready"], false);
    assert!(
        body["checks"]["database"]
            .as_str()
            .unwrap()
            .starts_with("error"),
        "{body}"
    );
}

#[tokio::test]
async fn readyz_states() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let state = state_for(Db::from_pool(db.pool.clone()));
    let router = internal_router(state.clone());

    let body = json(get(&router, "/readyz").await).await;
    assert_eq!(body["ready"], false);
    assert_eq!(body["checks"]["database"], "ok");
    assert_eq!(body["checks"]["migrations"], "pending");
    assert_eq!(body["checks"]["config"], "not loaded");

    state.readiness.set_migrations_current(true);
    state.readiness.set_config_loaded(true);
    let resp = get(&router, "/readyz").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["ready"], true);

    state.readiness.set_shutting_down();
    let resp = get(&router, "/readyz").await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(resp).await["checks"]["shutdown"], "draining");
}

#[tokio::test]
async fn metrics_only_on_internal_listener() {
    let state = offline_state();
    let internal = internal_router(state.clone());
    let resp = get(&internal, "/metrics").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    // On the public listener /metrics is just another SPA route, never the metrics text.
    let resp = get(&public_router(state), "/metrics").await;
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
}

#[tokio::test]
async fn site_config_schema_is_served() {
    let resp = get(&public_router(offline_state()), "/.api/site-config-schema").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = json(resp).await;
    assert!(schema["properties"]["externalURL"].is_object(), "{schema}");
}

#[tokio::test]
async fn resources_schema_is_served() {
    let resp = get(&public_router(offline_state()), "/.api/resources-schema").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = json(resp).await;
    assert!(schema["properties"]["connections"].is_object(), "{schema}");
}

#[tokio::test]
async fn settings_schema_is_served() {
    let resp = get(&public_router(offline_state()), "/.api/settings-schema").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = json(resp).await;
    let p = &schema["properties"]["search.defaultPatternType"];
    assert!(p.is_object(), "{schema}");
    assert_eq!(p["default"], "keyword", "{schema}");
}

#[tokio::test]
async fn spa_fallback_and_index_caching() {
    let router = public_router(offline_state());
    for path in [
        "/",
        "/search",
        "/site-admin/configuration",
        "/gitlab.com/a/b/-/blob/x.rs",
    ] {
        let resp = get(&router, path).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(resp.headers()[header::CACHE_CONTROL], CACHE_REVALIDATE);
    }

    let resp = get(&router, "/").await;
    let etag = resp.headers()[header::ETAG].clone();
    let req = Request::get("/repos")
        .header(header::IF_NONE_MATCH, etag.clone())
        .body(Body::empty())
        .expect("request");
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::ETAG], etag);
    assert!(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
}

#[tokio::test]
async fn unknown_api_paths_are_json_404() {
    let router = public_router(offline_state());
    for path in [
        "/.api/nope",
        "/.api",
        "/-/nope",
        "/_app/immutable/missing.js",
    ] {
        let resp = get(&router, path).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json(resp).await["error"], "not found");
    }
    let req = Request::post("/.api/nope")
        .body(Body::empty())
        .expect("request");
    assert_eq!(send(&router, req).await.status(), StatusCode::NOT_FOUND);
    let req = Request::post("/search")
        .body(Body::empty())
        .expect("request");
    assert_eq!(
        send(&router, req).await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn immutable_assets_are_cached_forever() {
    if !sourcegull_web_assets::ui_embedded() {
        return;
    }
    // Find an immutable chunk referenced by index.html.
    let index = sourcegull_web_assets::index_html();
    let html = String::from_utf8_lossy(&index.bytes);
    let start = html.find("/_app/immutable/").expect("immutable reference");
    let end = start + html[start..].find('"').expect("closing quote");
    let path = &html[start..end];

    let router = public_router(offline_state());
    let resp = get(&router, path).await;
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], CACHE_IMMUTABLE);
    let etag = resp.headers()[header::ETAG].clone();

    let req = Request::get(path)
        .header(header::IF_NONE_MATCH, etag)
        .body(Body::empty())
        .expect("request");
    let resp = send(&router, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::CACHE_CONTROL], CACHE_IMMUTABLE);
}

#[tokio::test]
async fn gzip_is_applied_when_accepted() {
    let router = public_router(offline_state());
    let req = Request::get("/.api/site-config-schema")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .expect("request");
    let resp = send(&router, req).await;
    assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn merged_routes_share_state_and_middleware() {
    use axum::extract::State;
    use axum::routing::get as get_route;
    let extra: Router<AppState> = Router::new().route(
        "/.api/version",
        get_route(|State(s): State<AppState>| async move { s.config.version().to_string() }),
    );
    let router = sourcegull_http_api::finish(
        sourcegull_http_api::public_routes().merge(extra),
        offline_state(),
    );
    let resp = get(&router, "/.api/version").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));
    // The fallback still applies to the merged router.
    assert_eq!(
        get(&router, "/.api/other").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[test]
fn csp_allows_the_bootstrap_script_only_by_hash() {
    let csp = sourcegull_http_api::content_security_policy();
    assert!(csp.contains("frame-ancestors 'none'"));
    let script_src = csp
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with("script-src"))
        .unwrap();
    assert!(!script_src.contains("'unsafe-inline'"), "{csp}");
    assert!(!script_src.contains("'unsafe-eval'"), "{csp}");
    // The search query parser is WebAssembly.
    assert!(script_src.contains("'wasm-unsafe-eval'"), "{csp}");
    if sourcegull_web_assets::ui_embedded() {
        assert!(csp.contains("'sha256-"), "{csp}");
    }
}
