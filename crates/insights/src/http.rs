//! `GET /.api/insights/export/{id}`: a view's data as a ZIP of one CSV
//! ([`crate::export`]). `{id}` is the view's node ID. Signed-in users only (401); a view the
//! viewer does not see is 404, so its existence does not leak.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sourcegull_authn::access::require_signed_in;
use sourcegull_authn::{AccessError, Actor};
use sourcegull_http_extract::{MaybeActor, access_error_response, error_response};
use sourcegull_services::AppState;

use crate::access;
use crate::export::{archive_name, csv, rows, zip};
use crate::store::InsightStore;
use crate::views::{View, ViewStore, parse_view_node_id};

/// The export route, to merge into the public router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/.api/insights/export/{id}", get(export))
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "exporting insight data");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Failed to export data.",
    )
}

/// The signed-in actor and the view with node ID `id` if they see it; an ID that is not a
/// view's is not found.
async fn visible<'a>(
    views: &ViewStore,
    actor: Option<&'a Actor>,
    id: &str,
) -> Result<(&'a Actor, View), AccessError> {
    let actor = require_signed_in(actor)?;
    let who = access::principal(views, Some(actor)).await?;
    let unique_id = parse_view_node_id(id).ok_or(AccessError::NotFound("Trend"))?;
    // 🚨 SECURITY: only views the viewer sees, as the trends API shows them.
    Ok((actor, access::visible_view(views, &who, &unique_id).await?))
}

async fn export(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Path(id): Path<String>,
) -> Response {
    let views = ViewStore::new(state.db.pool().clone());
    let (actor, view) = match visible(&views, actor.as_ref(), &id).await {
        Ok(v) => v,
        Err(AccessError::Internal(e)) => return internal(e),
        Err(e) => return access_error_response(&e),
    };
    let access = match state.authz.access(Some(actor)).await {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    let catalog = match state.search_catalog.get(&state.db).await {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let only = match crate::filters::resolve(
        &state.db,
        &catalog,
        &view.filters,
        Some(actor.user_id),
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, "INVALID_INPUT", &e);
        }
    };
    let names: HashMap<i32, String> = catalog
        .repos()
        .iter()
        .map(|r| (r.id, r.name.clone()))
        .collect();
    let store = InsightStore::new(&state.db);
    let data = match rows(&store, &view, &access, only.as_ref(), &names).await {
        Ok(r) => csv(&r),
        Err(e) => return internal(e),
    };
    let name = archive_name(&view.title, chrono::Utc::now());
    let archive = zip(&format!("{name}.csv"), data.as_bytes());
    let mut resp = archive.into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.zip\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
