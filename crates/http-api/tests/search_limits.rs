//! Site config `search.limits` in `GET /.api/search/stream`, against the test Postgres and a
//! real repo node: `streamingMaxResults` is the result count without `count:`, and
//! `defaultTimeoutSeconds`/`maxTimeoutSeconds` bound a search whose repo node never answers.
//! The limits are read at each search, so a reloaded site config applies at once.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SearchLimits, SiteConfig, SiteConfigHandle};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tower::ServiceExt;

const REPO: &str = "gitlab.example.com/acme/alpha";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    node: RepoNode,
    config: SiteConfigHandle,
    router: Router,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let src = SourceRepo::new();
    let files: Vec<(String, &str)> = (0..5)
        .map(|i| (format!("f{i}.txt"), "a needle here\n"))
        .collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    src.commit("one", &files);
    let node = RepoNode::start(src.remote()).await;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(REPO)
    .fetch_one(db.pool())
    .await
    .unwrap();
    node.clone_repo(RepoId(id), REPO).await;

    let config = ConfigHandle::new(1, site(SearchLimits::default()));
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone());
    Some(Env {
        _t: t,
        _src: src,
        node,
        config,
        router: public_router(state),
    })
}

fn site(search_limits: SearchLimits) -> SiteConfig {
    SiteConfig {
        auth_public: true,
        search_limits,
        ..SiteConfig::default()
    }
}

fn encode(q: &str) -> String {
    let mut out = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The SSE events of a search: (event name, JSON data).
async fn search(router: &Router, q: &str) -> Vec<(String, Value)> {
    let uri = format!("/.api/search/stream?q={}", encode(q));
    let resp = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&body)
        .split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_owned());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            event.map(|e| (e, serde_json::from_str(&data).unwrap_or(Value::Null)))
        })
        .collect()
}

fn match_count(events: &[(String, Value)]) -> usize {
    events
        .iter()
        .filter(|(e, _)| e == "matches")
        .map(|(_, d)| d.as_array().map_or(0, Vec::len))
        .sum()
}

fn skipped(events: &[(String, Value)]) -> Vec<String> {
    let progress = events.iter().rev().find(|(e, _)| e == "progress").unwrap();
    progress.1["skipped"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| s["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// A gRPC address that accepts connections and never answers.
async fn silent_node() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    addr
}

#[tokio::test]
async fn streaming_search_honours_search_limits() {
    let Some(env) = setup().await else { return };
    let q = "needle type:file";

    // Defaults: all five files.
    let events = search(&env.router, q).await;
    assert_eq!(match_count(&events), 5, "{events:?}");
    assert!(!skipped(&events).contains(&"display".to_owned()));

    // A reloaded `streamingMaxResults` is the result count without `count:`.
    env.config.store(
        2,
        site(SearchLimits {
            streaming_max_results: 2,
            ..SearchLimits::default()
        }),
    );
    let events = search(&env.router, q).await;
    assert_eq!(match_count(&events), 2, "{events:?}");
    assert!(skipped(&events).contains(&"display".to_owned()));
    // `count:` overrides it.
    let events = search(&env.router, &format!("{q} count:4")).await;
    assert_eq!(match_count(&events), 4, "{events:?}");

    // A repo node that never answers: the search stops at `defaultTimeoutSeconds`, and
    // `timeout:` is clamped to `maxTimeoutSeconds`.
    env.config.store(
        3,
        site(SearchLimits {
            default_timeout_seconds: 1,
            max_timeout_seconds: 2,
            ..SearchLimits::default()
        }),
    );
    env.node
        .clients
        .set_router(sourcegull_rpc::Router::new([silent_node().await]));
    for (query, expected) in [
        (q.to_owned(), Duration::from_secs(1)),
        (format!("{q} timeout:30s"), Duration::from_secs(2)),
    ] {
        let started = Instant::now();
        let events = search(&env.router, &query).await;
        let took = started.elapsed();
        assert!(
            skipped(&events).contains(&"shard-timeout".to_owned()),
            "{query}: {events:?}"
        );
        assert!(
            took >= expected && took < expected + Duration::from_secs(3),
            "{query}: took {took:?}, expected about {expected:?}"
        );
    }
}
