//! `GET /.api/compute/stream`: Compute over the streaming search.
//!
//! Runs the compute query with [`sourcegull_services::compute::spawn_compute`] and streams its
//! events: `results` (a JSON array of `MatchContext` / `Text` objects), `progress` (the
//! search's), `alert`, `error`, and a final `done`. A compute query runs for at most
//! [`MAX_DURATION`](sourcegull_services::compute::MAX_DURATION), then ends with an
//! "Incomplete data" alert.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Deserialize;
use sourcegull_compute::ComputeQuery;
use sourcegull_http_extract::{MaybeActor, error_response};
use sourcegull_services::AppState;
use sourcegull_services::compute::spawn_compute;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    #[serde(default)]
    q: String,
    /// Accepted when it is an integer, and ignored.
    display: Option<String>,
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

/// `GET /.api/compute/stream?q=…`.
pub async fn stream(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Query(params): Query<StreamParams>,
) -> Response {
    if actor.is_none() && !state.config.load().value.auth_public {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "You must be signed in.",
        );
    }
    if params.q.is_empty() {
        return bad_request("no query found".to_owned());
    }
    if let Some(d) = params.display.as_deref()
        && d.parse::<i64>().is_err()
    {
        return bad_request(format!("display must be an integer, got {d:?}"));
    }
    let query = match ComputeQuery::parse(&params.q) {
        Ok(q) => Arc::new(q),
        Err(e) => return bad_request(e.to_string()),
    };
    let mut rx = spawn_compute(&state.search_backend(), actor.as_ref(), query);
    let events = futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
        .map(|e| SseEvent::default().event(e.name()).data(e.data()))
        .chain(futures::stream::once(async {
            SseEvent::default().event("done").data("{}")
        }))
        .map(Ok::<_, Infallible>);
    let mut resp = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
