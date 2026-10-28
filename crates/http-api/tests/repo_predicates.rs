//! Repository predicates in search through `GET /.api/search/stream`, against the
//! test Postgres and a real repo node: `repo:has.file()`, `repo:has.path()`,
//! `repo:has.content()` (negated too, indexed and unindexed), `repo:has.commit.after()` and
//! `rev:at.time()`, a private repository the anonymous viewer may not read staying
//! invisible, and alerts for predicates that cannot be evaluated.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tower::ServiceExt;

const ALPHA: &str = "gitlab.example.com/acme/alpha";
const BETA: &str = "gitlab.example.com/acme/beta";
const SECRET: &str = "gitlab.example.com/acme/secret";

struct Env {
    _t: TestDb,
    _srcs: Vec<SourceRepo>,
    node: RepoNode,
    ids: HashMap<&'static str, RepoId>,
    router: Router,
    /// Committer times of alpha's two commits.
    alpha_times: (i64, i64),
}

fn commit_time(src: &SourceRepo) -> i64 {
    src.git(&["log", "-1", "--format=%ct"]).parse().unwrap()
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    // alpha: go.mod first, a TODO in a later commit (so it is newer than beta's only one).
    let alpha = SourceRepo::new();
    alpha.commit("one", &[("go.mod", "module alpha\n")]);
    let t1 = commit_time(&alpha);
    alpha.commit("two", &[("src/new.rs", "// TODO later\n")]);
    let t2 = commit_time(&alpha);
    let beta = SourceRepo::new();
    beta.commit("one", &[("README.md", "hello TODO\n")]);
    let secret = SourceRepo::new();
    secret.commit(
        "one",
        &[("go.mod", "module secret\n"), ("README.md", "TODO\n")],
    );

    let remotes: HashMap<String, _> = [
        (ALPHA, alpha.remote()),
        (BETA, beta.remote()),
        (SECRET, secret.remote()),
    ]
    .into_iter()
    .map(|(n, r)| (n.to_owned(), r))
    .collect();
    let node = RepoNode::start_with(move |r| Ok(remotes[&r.name].clone())).await;
    let mut ids = HashMap::new();
    for (i, (name, private)) in [(ALPHA, false), (BETA, false), (SECRET, true)]
        .into_iter()
        .enumerate()
    {
        let id: i32 = sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id, \
             private) VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/', $3) RETURNING id",
        )
        .bind(name)
        .bind(i.to_string())
        .bind(private)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let id = RepoId(id);
        node.clone_repo(id, name).await;
        ids.insert(name, id);
    }

    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_public: true,
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone());
    Some(Env {
        _t: t,
        _srcs: vec![alpha, beta, secret],
        node,
        ids,
        router: public_router(state),
        alpha_times: (t1, t2),
    })
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

/// Sorted, deduplicated `repo` or `repo:path` of every match; fails on alerts, errors and
/// skipped repositories.
async fn results(router: &Router, q: &str) -> Vec<String> {
    let events = search(router, q).await;
    assert!(
        !events.iter().any(|(e, _)| e == "alert" || e == "error"),
        "{q}: {events:?}"
    );
    let progress = events.iter().rev().find(|(e, _)| e == "progress").unwrap();
    assert!(
        progress.1["skipped"]
            .as_array()
            .is_none_or(|s| s.iter().all(|s| s["reason"] == "display")),
        "{q}: {progress:?}"
    );
    let mut out: Vec<String> = events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| d.as_array().cloned().unwrap_or_default())
        .map(|m| {
            let repo = m["repository"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap();
            match m["path"].as_str() {
                Some(p) => format!("{repo}:{p}"),
                None => repo.to_owned(),
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

async fn alert(router: &Router, q: &str) -> Value {
    let events = search(router, q).await;
    events
        .into_iter()
        .find(|(e, _)| e == "alert")
        .unwrap_or_else(|| panic!("{q}: no alert"))
        .1
}

fn rfc3339(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The `repo:has.file()` family; `extra` is added to every query (`index:` variants).
async fn check_has_file(env: &Env, extra: &str) {
    let r = &env.router;
    let q = |s: &str| format!("{s} {extra}");
    // Repositories (no pattern): the private repository's go.mod is not visible.
    assert_eq!(
        results(r, &q("repo:has.file(path:go\\.mod)")).await,
        ["alpha"]
    );
    assert_eq!(
        results(r, &q("repo:has.file(path:go\\.mod content:ALPHA)")).await,
        ["alpha"]
    );
    assert!(
        results(r, &q("repo:has.file(path:go\\.mod content:beta)"))
            .await
            .is_empty()
    );
    assert_eq!(results(r, &q("repo:has.path(readme)")).await, ["beta"]);
    assert_eq!(results(r, &q("repo:has.content(module)")).await, ["alpha"]);
    assert_eq!(results(r, &q("-repo:has.path(go\\.mod)")).await, ["beta"]);
    assert_eq!(
        results(r, &q("repo:has.path(go\\.mod) repo:has.content(TODO)")).await,
        ["alpha"]
    );
    assert!(
        results(r, &q("repo:has.path(go\\.mod) -repo:has.content(TODO)"))
            .await
            .is_empty()
    );
    // With a pattern: file results only from the repositories that pass.
    assert_eq!(
        results(r, &q("repo:has.path(go\\.mod) TODO type:file")).await,
        ["alpha:src/new.rs"]
    );
    assert_eq!(
        results(r, &q("-repo:has.path(go\\.mod) TODO type:file")).await,
        ["beta:README.md"]
    );
    // Repository names match the pattern too.
    assert_eq!(
        results(r, &q("repo:has.content(hello) TODO")).await,
        ["beta:README.md"]
    );
    assert_eq!(
        results(r, &q("repo:has.content(hello) bet")).await,
        ["beta"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn has_file_unindexed_and_indexed() {
    let Some(env) = setup().await else { return };
    check_has_file(&env, "").await;
    check_has_file(&env, "index:no").await;
    for name in [ALPHA, BETA, SECRET] {
        env.node.index_repo(env.ids[name], name, &[]);
    }
    check_has_file(&env, "").await;
    check_has_file(&env, "index:only").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_after_and_rev_at_time() {
    let Some(env) = setup().await else { return };
    let r = &env.router;
    let (t1, t2) = env.alpha_times;
    // alpha's second commit is newer than everything in beta.
    let after = rfc3339(t2 - 1);
    assert_eq!(
        results(r, &format!("repo:has.commit.after({after})")).await,
        ["alpha"]
    );
    assert_eq!(
        results(r, &format!("-repo:has.commit.after({after})")).await,
        ["beta"]
    );
    assert_eq!(
        results(r, &format!("repo:has.commit.after({after}) TODO type:file")).await,
        ["alpha:src/new.rs"]
    );
    assert_eq!(
        results(r, "repo:has.commit.after(2000-01-01)").await,
        ["alpha", "beta"]
    );

    // rev:at.time(): alpha at its first commit has no TODO yet, at its second it has.
    let at = |t: i64, rest: &str| format!("repo:alpha rev:at.time({}{rest}) TODO", rfc3339(t));
    assert!(results(r, &at(t1, "")).await.is_empty());
    assert_eq!(results(r, &at(t2, "")).await, ["alpha:src/new.rs"]);
    assert_eq!(
        results(r, &at(t2 + 3600, ", main")).await,
        ["alpha:src/new.rs"]
    );
    assert_eq!(
        results(
            r,
            &format!("repo:alpha rev:at.time({}) file:go", rfc3339(t1))
        )
        .await,
        ["alpha:go.mod"]
    );
    // Before the first commit there is nothing to search.
    assert!(results(r, &at(t1 - 3600, "")).await.is_empty());
    // Every visible repository at that time; the private one stays invisible.
    assert_eq!(
        results(r, &format!("rev:at.time({}) TODO type:file", rfc3339(t2))).await,
        ["alpha:src/new.rs", "beta:README.md"]
    );
    // The match is at the commit the time names.
    let events = search(r, &at(t2, "")).await;
    let m = events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| d.as_array().cloned().unwrap_or_default())
        .next()
        .unwrap();
    assert_eq!(m["commit"].as_str().map(str::len), Some(40));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn predicate_alerts() {
    let Some(env) = setup().await else { return };
    let r = &env.router;
    let a = alert(r, "repo:has.commit.after(whenever) foo").await;
    assert_eq!(a["title"], "Unable To Process Query");
    assert!(a["description"].as_str().unwrap().contains("invalid date"));
    let a = alert(r, "-rev:at.time(yesterday) foo").await;
    assert!(
        a["description"]
            .as_str()
            .unwrap()
            .contains("does not support negation"),
        "{a}"
    );
    let a = alert(r, "repo:has.path([) foo").await;
    assert!(
        a["description"]
            .as_str()
            .unwrap()
            .contains("repo:has.path()"),
        "{a}"
    );
}
