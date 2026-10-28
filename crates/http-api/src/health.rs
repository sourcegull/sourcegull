//! `/healthz`, `/readyz`, `/metrics` and the configuration schemas (`/.api/site-config-schema`,
//! `/.api/settings-schema`, `/.api/resources-schema`).

use std::sync::LazyLock;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use sourcegull_services::AppState;

/// How long `/readyz` waits for the database before reporting it unreachable.
pub const READYZ_DB_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /healthz`: the process is alive and serving HTTP. Never touches dependencies.
pub async fn healthz() -> Response {
    let mut resp = Json(serde_json::json!({ "status": "ok" })).into_response();
    no_store(&mut resp);
    resp
}

/// The `/readyz` response body.
#[derive(Debug, Serialize)]
struct ReadyBody {
    ready: bool,
    checks: Checks,
}

#[derive(Debug, Serialize)]
struct Checks {
    database: String,
    migrations: &'static str,
    config: &'static str,
    shutdown: &'static str,
}

/// `GET /readyz`: 200 when the database is reachable, migrations are current, the site config is
/// loaded and the process is not shutting down; 503 otherwise. The body lists each check.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let r = &state.readiness;
    let db_query = sourcegull_db::sqlx::query("SELECT 1").execute(state.db.pool());
    let database = match tokio::time::timeout(READYZ_DB_TIMEOUT, db_query).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("error: {e}")),
        Err(_) => Err(format!("error: no response within {READYZ_DB_TIMEOUT:?}")),
    };
    let ready =
        database.is_ok() && r.migrations_current() && r.config_loaded() && !r.shutting_down();
    let body = ReadyBody {
        ready,
        checks: Checks {
            database: database.err().unwrap_or_else(|| "ok".to_owned()),
            migrations: if r.migrations_current() {
                "ok"
            } else {
                "pending"
            },
            config: if r.config_loaded() {
                "ok"
            } else {
                "not loaded"
            },
            shutdown: if r.shutting_down() { "draining" } else { "ok" },
        },
    };
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let mut resp = (status, Json(body)).into_response();
    no_store(&mut resp);
    resp
}

/// `GET /metrics` (internal listener only): Prometheus text exposition format.
pub async fn metrics(State(state): State<AppState>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

static SCHEMA_JSON: LazyLock<Vec<u8>> = LazyLock::new(|| {
    serde_json::to_vec(&sourcegull_config::site_config_schema()).unwrap_or_else(|_| b"{}".to_vec())
});

/// `GET /.api/site-config-schema`: JSON Schema of the site config, for the in-UI editor.
pub async fn site_config_schema() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SCHEMA_JSON.as_slice(),
    )
        .into_response()
}

static SETTINGS_SCHEMA_JSON: LazyLock<Vec<u8>> = LazyLock::new(|| {
    serde_json::to_vec(&sourcegull_config::settings_schema()).unwrap_or_else(|_| b"{}".to_vec())
});

/// `GET /.api/settings-schema`: JSON Schema of user, org and global settings, for the
/// settings reference page and editor.
pub async fn settings_schema() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SETTINGS_SCHEMA_JSON.as_slice(),
    )
        .into_response()
}

static RESOURCES_SCHEMA_JSON: LazyLock<Vec<u8>> = LazyLock::new(|| {
    serde_json::to_vec(&sourcegull_config::resources::resources_schema())
        .unwrap_or_else(|_| b"{}".to_vec())
});

/// `GET /.api/resources-schema`: JSON Schema of the resources file (`SOURCEGULL_RESOURCES_FILE`),
/// for editors (`"$schema"` in the file).
pub async fn resources_schema() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        RESOURCES_SCHEMA_JSON.as_slice(),
    )
        .into_response()
}

fn no_store(resp: &mut Response) {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}
