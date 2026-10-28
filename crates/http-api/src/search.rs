//! `GET /.api/search/stream`: the streaming search API.
//!
//! Runs the search with [`sourcegull_search_web::run`] and streams its events as server-sent
//! events, adding `filters` (facet counts, at most every [`FILTERS_INTERVAL`] and once at the
//! end) and a final `done`. Slow searches are logged ([`sourcegull_services::slow_log`]) and every
//! search records a usage event.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use sourcegull_db::Db;
use sourcegull_http_extract::{MaybeActor, error_response};
use sourcegull_search::Facets;
use sourcegull_search::stream::Event;
use sourcegull_search_web::{StreamParams, run};
use sourcegull_services::AppState;

/// Minimum time between `filters` events.
const FILTERS_INTERVAL: Duration = Duration::from_millis(500);

/// Logs the search when it was slow, once its stream ends (or the client went away).
struct SlowSearch {
    /// Threshold, query, pattern type and user, when slow searches are logged.
    slow: Option<(u32, String, Option<String>, Option<i64>)>,
    db: Db,
    started_at: chrono::DateTime<chrono::Utc>,
    started: std::time::Instant,
}

impl Drop for SlowSearch {
    fn drop(&mut self) {
        let Some((threshold, query, pattern_type, user_id)) = self.slow.take() else {
            return;
        };
        let elapsed = self.started.elapsed();
        if !sourcegull_services::slow_log::exceeded(Some(threshold), elapsed) {
            return;
        }
        sourcegull_services::slow_log::record(
            &self.db,
            sourcegull_db::NewSlowRequest {
                started_at: self.started_at,
                duration_ms: elapsed.as_secs_f64() * 1000.0,
                kind: sourcegull_db::SlowRequestKind::Search,
                user_id,
                name: "search".to_owned(),
                source: "stream".to_owned(),
                repository: None,
                variables: serde_json::json!({ "patternType": pattern_type }).to_string(),
                errors: Vec::new(),
                query,
                filepath: None,
            },
        );
    }
}

/// `GET /.api/search/stream?q=…&t=keyword&display=1500&max-line-len=…`.
pub async fn stream(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    Query(params): Query<StreamParams>,
) -> Response {
    if actor.is_none() && !state.config.load().value.auth_public {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "You must be signed in.",
        );
    }
    // The usage event of this search, recorded when it finishes.
    let mut usage = sourcegull_db::NewUsageEvent {
        name: sourcegull_analytics::names::SEARCH_RESULTS_QUERIED.to_owned(),
        user_id: actor.as_ref().map(|a| a.user_id),
        anonymous_user_id: if actor.is_none() {
            headers
                .get_all(header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(sourcegull_analytics::names::anonymous_uid_from_cookie)
                .unwrap_or_default()
        } else {
            String::new()
        },
        source: sourcegull_analytics::names::BACKEND.to_owned(),
        ..Default::default()
    };
    let search_type = sourcegull_analytics::names::search_type(&params.q, params.t.as_deref());
    let pattern_type = params.t.clone().unwrap_or_default();
    let chunk_matches = params
        .cm
        .as_deref()
        .is_some_and(|v| matches!(v, "t" | "true" | "1"));
    let (tx, mut results) = tokio::sync::mpsc::channel::<Event>(16);
    let (out, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
    // The slow search log times the search to its last event.
    let slow = state
        .config
        .load()
        .value
        .observability_log_slow_searches
        .map(|threshold| {
            let user_id = actor.as_ref().map(|a| a.user_id.get());
            (threshold, params.q.clone(), params.t.clone(), user_id)
        });
    let (db, started_at, started) = (
        state.db.clone(),
        chrono::Utc::now(),
        std::time::Instant::now(),
    );
    tokio::spawn(run(state.search_backend(), params, actor, tx));
    tokio::spawn(async move {
        let _slow_guard = SlowSearch {
            slow,
            db: db.clone(),
            started_at,
            started,
        };
        // Forward events, counting facets; send filters at most every
        // FILTERS_INTERVAL and once more at the end.
        let mut facets = Facets::new();
        let mut dirty = false;
        let mut last = std::time::Instant::now();
        let mut result_count = 0usize;
        let mut closed = false;
        while let Some(event) = results.recv().await {
            if let Event::Matches(ms) = &event {
                ms.iter().for_each(|m| facets.observe(m));
                result_count += ms.len();
                dirty = true;
            }
            if out.send(event).await.is_err() {
                // The client went away; dropping `results` stops the search.
                closed = true;
                break;
            }
            if dirty && last.elapsed() >= FILTERS_INTERVAL {
                let _ = out.send(Event::Filters(facets.filters())).await;
                (dirty, last) = (false, std::time::Instant::now());
            }
        }
        if !closed {
            if dirty {
                let _ = out.send(Event::Filters(facets.filters())).await;
            }
            let _ = out.send(Event::Done).await;
        }
        // Abandoned searches count without a latency.
        usage.argument = serde_json::json!({
            "type": search_type,
            "patternType": pattern_type,
            "resultCount": result_count,
        });
        if !closed {
            usage.argument["durationMs"] = serde_json::json!(started.elapsed().as_millis() as u64);
        }
        if let Err(e) = db.event_logs().insert(std::slice::from_ref(&usage)).await {
            tracing::warn!(error = %e, "recording the search usage event");
        }
    });
    let events = futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(move |e| {
        let data = if chunk_matches {
            e.data()
        } else {
            e.data_with_line_matches()
        };
        Ok::<_, Infallible>(SseEvent::default().event(e.name()).data(data))
    });
    let mut resp = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
