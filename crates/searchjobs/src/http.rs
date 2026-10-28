//! Downloads of a search job:
//! `GET /.api/search/export/{id}.jsonl` (the results), `{id}.csv` (the results converted to
//! CSV on the fly) and `{id}.log` (one CSV row per searched repository revision). Only the
//! job's creator and site admins may download; others get 403, unknown jobs 404.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sourcegull_authn::access::require_signed_in;
use sourcegull_http_extract::{MaybeActor, access_error_response, error_response};
use sourcegull_services::AppState;

use crate::SearchJobStore;
use crate::access::require_job_access;
use crate::export::{log_csv, results_csv, results_jsonl};

/// The download routes, to merge into the public router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/.api/search/export/{file}", get(download))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Jsonl,
    Csv,
    Log,
}

/// `{id}.jsonl`, `{id}.csv`, `{id}.log` (or `{id}.log.csv`).
fn parse_file(file: &str) -> Option<(i64, Kind)> {
    let (id, kind) = if let Some(id) = file.strip_suffix(".jsonl") {
        (id, Kind::Jsonl)
    } else if let Some(id) = file.strip_suffix(".log.csv") {
        (id, Kind::Log)
    } else if let Some(id) = file.strip_suffix(".log") {
        (id, Kind::Log)
    } else {
        (file.strip_suffix(".csv")?, Kind::Csv)
    };
    Some((id.parse().ok()?, kind))
}

/// The download file name: `search-jobs_<id>_<YYYY-MM-DD_HHMMSS>`.
fn filename(id: i64) -> String {
    format!(
        "search-jobs_{id}_{}",
        chrono::Utc::now().format("%Y-%m-%d_%H%M%S")
    )
}

async fn download(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Path(file): Path<String>,
) -> Response {
    let actor = match require_signed_in(actor.as_ref()) {
        Ok(a) => a,
        Err(e) => return access_error_response(&e),
    };
    let Some((id, kind)) = parse_file(&file) else {
        return error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found.");
    };
    let store = SearchJobStore::new(state.db.pool().clone());
    // 🚨 SECURITY: only the creator and site admins may read a job's results and log.
    if let Err(e) = require_job_access(&store, actor, id).await {
        return access_error_response(&e);
    }
    let (body, content_type, name) = match kind {
        Kind::Log => (
            log_csv(store, id),
            "text/csv",
            format!("{}.log.csv", filename(id)),
        ),
        Kind::Jsonl | Kind::Csv => {
            let Some(blobs) = state.blobs.clone() else {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "UNAVAILABLE",
                    "No blob store is configured.",
                );
            };
            if kind == Kind::Jsonl {
                (
                    results_jsonl(store, blobs, id),
                    "application/jsonlines",
                    format!("{}.jsonl", filename(id)),
                )
            } else {
                let base = state.config.load().value.external_url.clone();
                (
                    results_csv(store, blobs, id, base),
                    "text/csv",
                    format!("{}.csv", filename(id)),
                )
            }
        }
    };
    let mut resp = Body::from_stream(body).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(parse_file("12.jsonl"), Some((12, Kind::Jsonl)));
        assert_eq!(parse_file("12.csv"), Some((12, Kind::Csv)));
        assert_eq!(parse_file("12.log"), Some((12, Kind::Log)));
        assert_eq!(parse_file("12.log.csv"), Some((12, Kind::Log)));
        assert_eq!(parse_file("x.jsonl"), None);
        assert_eq!(parse_file("12.txt"), None);
        assert!(filename(3).starts_with("search-jobs_3_"));
    }
}
