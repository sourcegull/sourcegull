//! Raw files, directory archives, streaming blame and the SPA fallback for repository pages,
//! against the test Postgres and a real repo node started in-process over a temp git repo.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::public_router;
use sourcegull_http_api::repo_content::{BlameHunkEvent, RAW_CSP};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{BrowseRepoOids, RepoNode, SourceRepo, browse_repo};
use sourcegull_types::RepoId;
use tower::ServiceExt;

const REPO: &str = "gitlab.example.com/acme/demo";
const UNCLONED: &str = "gitlab.example.com/acme/uncloned";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    node: RepoNode,
    id: RepoId,
    oids: BrowseRepoOids,
    router: Router,
    private_router: Router,
}

async fn insert_repo(db: &Db, name: &str, ext: &str) -> RepoId {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .bind(ext)
    .fetch_one(db.pool())
    .await
    .unwrap();
    RepoId(id)
}

fn state(db: &Db, public: bool, node: &RepoNode) -> AppState {
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_public: public,
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
    AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone())
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let (src, oids) = browse_repo();
    let node = RepoNode::start(src.remote()).await;
    let id = insert_repo(&db, REPO, "1").await;
    insert_repo(&db, UNCLONED, "2").await;
    node.clone_repo(id, REPO).await;
    Some(Env {
        router: public_router(state(&db, true, &node)),
        private_router: public_router(state(&db, false, &node)),
        _t: t,
        _src: src,
        node,
        id,
        oids,
    })
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap())
            .unwrap_or("")
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
}

async fn get(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = Request::get(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Resp {
        status,
        headers,
        body,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_files_and_archives() {
    let Some(env) = setup().await else { return };
    let r = &env.router;

    let main = get(r, &format!("/{REPO}@main/-/raw/src/main.rs"), &[]).await;
    assert_eq!(main.status, StatusCode::OK);
    assert_eq!(
        main.header(header::CONTENT_TYPE),
        "text/plain; charset=utf-8"
    );
    assert_eq!(main.header(header::CONTENT_SECURITY_POLICY), RAW_CSP);
    assert_eq!(main.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(main.header(header::CACHE_CONTROL), "private, no-cache");
    assert!(main.text().contains("hello, world"));
    let etag = main.header(header::ETAG).to_owned();
    assert!(etag.starts_with('"'));

    let cached = get(
        r,
        &format!("/{REPO}@main/-/raw/src/main.rs"),
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);

    // `?rev=` when the URL has no `@rev`; a full oid makes the response cacheable.
    let old = get(r, &format!("/{REPO}/-/raw/src/main.rs?rev=main~2"), &[]).await;
    assert_eq!(old.text(), "fn main() {\n    println!(\"hello\");\n}\n");
    let pinned = get(r, &format!("/{REPO}@{}/-/raw/README.md", env.oids.c1), &[]).await;
    assert_eq!(pinned.status, StatusCode::OK);
    assert_eq!(
        pinned.header(header::CACHE_CONTROL),
        "private, max-age=86400"
    );
    let head = get(r, &format!("/{REPO}/-/raw/README.md"), &[]).await;
    assert_eq!(head.text(), "# Demo\n\nHello.\n");

    let img = get(r, &format!("/{REPO}/-/raw/img.png"), &[]).await;
    assert_eq!(img.header(header::CONTENT_TYPE), "image/png");
    assert_eq!(img.body.len(), 8);
    let dl = get(r, &format!("/{REPO}/-/raw/README.md?download"), &[]).await;
    assert!(
        dl.header(header::CONTENT_DISPOSITION)
            .starts_with("attachment; filename=\"README.md\"")
    );

    let listing = get(r, &format!("/{REPO}/-/raw/src"), &[]).await;
    assert_eq!(listing.status, StatusCode::OK);
    assert_eq!(listing.text(), "lib.rs\nmain.rs\n");
    let root = get(r, &format!("/{REPO}@main/-/raw"), &[]).await;
    assert_eq!(root.text(), "README.md\ndocs/\nimg.png\nsrc/\n");

    let zip = get(r, &format!("/{REPO}@main/-/raw/src?format=zip"), &[]).await;
    assert_eq!(zip.status, StatusCode::OK);
    assert_eq!(zip.header(header::CONTENT_TYPE), "application/zip");
    assert!(zip.body.starts_with(b"PK"));
    let disposition = zip.header(header::CONTENT_DISPOSITION);
    assert!(
        disposition.starts_with(&format!(
            "attachment; filename=\"demo-{}-src.zip\"",
            &env.oids.c3[..10]
        )),
        "{disposition}"
    );
    let tar = get(
        r,
        &format!("/{REPO}/-/raw/docs"),
        &[("accept", "application/x-tar")],
    )
    .await;
    assert_eq!(tar.header(header::CONTENT_TYPE), "application/x-tar");
    let text = tar.text();
    assert!(text.contains("docs/guide.md") && text.contains("docs/more.md"));
    assert!(!text.contains("src/main.rs"));

    // Errors.
    let cases = [
        (format!("/{REPO}/-/raw/nope.txt"), 404, "PATH_NOT_FOUND"),
        (
            format!("/{REPO}@nope/-/raw/README.md"),
            404,
            "REVISION_NOT_FOUND",
        ),
        (
            "/gitlab.example.com/x/y/-/raw/a".to_owned(),
            404,
            "REPO_NOT_FOUND",
        ),
        (
            format!("/{UNCLONED}/-/raw/README.md"),
            404,
            "REPO_NOT_CLONED",
        ),
    ];
    for (uri, status, code) in cases {
        let resp = get(r, &uri, &[]).await;
        assert_eq!(resp.status.as_u16(), status, "{uri}");
        assert_eq!(resp.json()["code"], code, "{uri}");
    }

    // Anonymous access needs `auth.public`.
    let denied = get(
        &env.private_router,
        &format!("/{REPO}/-/raw/README.md"),
        &[],
    )
    .await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_pages_fall_back_to_the_ui() {
    let Some(env) = setup().await else { return };
    for page in [
        format!("/{REPO}"),
        format!("/{REPO}@main/-/tree/src"),
        format!("/{REPO}@main/-/blob/src/main.rs"),
        format!("/{REPO}/-/commit/{}", env.oids.c2),
        format!("/{REPO}/-/commits/src"),
        format!("/{REPO}/-/compare/main...feature"),
        format!("/{REPO}/-/branches"),
        format!("/{REPO}/-/tags"),
    ] {
        let resp = get(&env.router, &page, &[]).await;
        assert_eq!(resp.status, StatusCode::OK, "{page}");
        assert_eq!(
            resp.header(header::CONTENT_TYPE),
            "text/html; charset=utf-8",
            "{page}"
        );
    }
    let api = get(&env.router, "/.api/nope", &[]).await;
    assert_eq!(api.status, StatusCode::NOT_FOUND);
}

/// Parses an SSE body into (event, data) pairs.
fn sse_events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = Vec::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_owned());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push(d.strip_prefix(' ').unwrap_or(d).to_owned());
                }
            }
            event.map(|e| (e, data.join("\n")))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blame_streams_hunks_then_done() {
    let Some(env) = setup().await else { return };
    let o = &env.oids;
    let resp = get(
        &env.router,
        &format!("/.api/blame/{REPO}@main/-/stream/src/main.rs"),
        &[],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(
        resp.header(header::CONTENT_TYPE)
            .starts_with("text/event-stream")
    );
    let events = sse_events(&resp.text());
    assert_eq!(
        events.last(),
        Some(&("done".to_owned(), "{}".to_owned())),
        "{events:?}"
    );
    let mut hunks: Vec<BlameHunkEvent> = events
        .iter()
        .filter(|(e, _)| e == "hunk")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<BlameHunkEvent>>(d).unwrap())
        .collect();
    hunks.sort_by_key(|h| h.start_line);
    let spans: Vec<(u32, u32, &str)> = hunks
        .iter()
        .map(|h| (h.start_line, h.end_line, h.commit_id.as_str()))
        .collect();
    assert_eq!(
        spans,
        [
            (1, 2, o.c1.as_str()),
            (2, 4, o.c2.as_str()),
            (4, 5, o.c1.as_str())
        ]
    );
    let h = &hunks[1];
    assert_eq!(h.message, "c2: greet");
    assert_eq!(h.filename, "src/main.rs");
    assert_eq!(h.author.name, "Ann Author");
    assert_eq!(h.author.email, "ann@example.com");
    assert!(h.author.date.starts_with("2023-11-14T"));
    assert_eq!(h.commit.parents, vec![o.c1.clone()]);
    assert_eq!(h.commit.url, format!("/{REPO}/-/commit/{}", o.c2));
    assert!(hunks[0].commit.parents.is_empty());

    // The raw JSON uses Go-style field names.
    let (_, first) = events.iter().find(|(e, _)| e == "hunk").unwrap();
    let raw: serde_json::Value = serde_json::from_str(first).unwrap();
    let keys = raw[0].as_object().unwrap();
    for k in [
        "commitID",
        "startLine",
        "endLine",
        "author",
        "message",
        "filename",
        "commit",
    ] {
        assert!(keys.contains_key(k), "{k} in {raw}");
    }
    assert!(raw[0]["author"].get("Name").is_some());

    // A line range and errors before streaming starts.
    let part = get(
        &env.router,
        &format!("/.api/blame/{REPO}/-/stream/src/main.rs?startLine=2&endLine=2"),
        &[],
    )
    .await;
    let part_hunks: Vec<BlameHunkEvent> = sse_events(&part.text())
        .iter()
        .filter(|(e, _)| e == "hunk")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<BlameHunkEvent>>(d).unwrap())
        .collect();
    assert_eq!(part_hunks.len(), 1);
    assert_eq!((part_hunks[0].start_line, part_hunks[0].end_line), (2, 3));

    let missing = get(
        &env.router,
        &format!("/.api/blame/{REPO}/-/stream/nope.rs"),
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let bad = get(&env.router, &format!("/.api/blame/{REPO}/-/blob/x"), &[]).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_streams_content_matches() {
    let Some(env) = setup().await else { return };
    let resp = get(&env.router, "/.api/search/stream?q=hello&cm=t", &[]).await;
    assert_eq!(resp.status, StatusCode::OK);
    let events = sse_events(&resp.text());
    assert_eq!(events.last().unwrap().0, "done");
    let matches: Vec<serde_json::Value> = events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<serde_json::Value>>(d).unwrap())
        .collect();
    let mut paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["README.md", "src/main.rs"]);
    let main = matches.iter().find(|m| m["path"] == "src/main.rs").unwrap();
    assert_eq!(main["type"], "content");
    assert_eq!(main["repository"], REPO);
    assert_eq!(main["commit"], env.oids.c3);
    assert_eq!(main["language"], "Rust");
    assert_eq!(
        main["chunkMatches"][0]["content"],
        "    println!(\"hello, world\");"
    );
    assert_eq!(main["chunkMatches"][0]["ranges"][0]["start"]["column"], 14);

    let (_, progress) = events.iter().rfind(|(e, _)| e == "progress").unwrap();
    let progress: serde_json::Value = serde_json::from_str(progress).unwrap();
    assert_eq!(progress["done"], true);
    assert_eq!(progress["matchCount"], 2);
    assert_eq!(progress["repositoriesCount"], 2);
    assert_eq!(progress["skipped"][0]["reason"], "repository-missing");
    let (_, filters) = events.iter().rfind(|(e, _)| e == "filters").unwrap();
    let filters: Vec<serde_json::Value> = serde_json::from_str(filters).unwrap();
    assert!(
        filters
            .iter()
            .any(|f| f["kind"] == "lang" && f["value"] == "lang:rust")
    );
    assert!(
        filters
            .iter()
            .any(|f| f["kind"] == "file" && f["label"] == ".md")
    );

    // Revisions, file filters and the display limit.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=fn+repo:demo%24+rev:feature+lang:rust&display=1",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["commit"], env.oids.f1);
    assert_eq!(m[0]["branches"][0], "feature");

    // Path search.
    let resp = get(&env.router, "/.api/search/stream?q=type:path+main", &[]).await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "path");
    assert_eq!(m[0]["path"], "src/main.rs");
    assert_eq!(m[0]["pathMatches"][0]["start"]["offset"], 4);

    // Structural search: the hole spans the balanced arguments.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=repo:demo%24+println!(:%5Bargs%5D)+patterntype:structural&cm=t",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["path"], "src/main.rs");
    let chunk = &m[0]["chunkMatches"][0];
    assert_eq!(chunk["content"], "    println!(\"hello, world\");");
    assert_eq!(chunk["ranges"][0]["start"]["column"], 4);
    assert_eq!(chunk["ranges"][0]["end"]["column"], 28);

    // Repository search.
    let resp = get(&env.router, "/.api/search/stream?q=type:repo+demo", &[]).await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "repo");
    assert_eq!(m[0]["repository"], REPO);

    // Commit search.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=repo:demo%24+type:commit+greet",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "commit");
    assert_eq!(m[0]["oid"], env.oids.c2);
    assert_eq!(m[0]["ranges"][0], serde_json::json!([1, 4, 5]));
    assert!(
        m[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("```COMMIT_EDITMSG\nc2: greet")
    );

    // Diff search: only changed lines match. The hunk
    // shows the matching line with one line of context and `@@ ... @@ `
    // keeps its space when git has no section heading.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=repo:demo%24+type:diff+file:%5Esrc/+world",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "commit");
    assert_eq!(m[0]["oid"], env.oids.c2);
    assert_eq!(
        m[0]["content"],
        "```diff\nsrc/main.rs src/main.rs\n@@ -2,1 +2,2 @@ \n-    println!(\"hello\");\n\
         +    println!(\"hello, world\");\n+    run();\n```"
    );
    assert_eq!(m[0]["ranges"], serde_json::json!([[4, 22, 5]]));

    // Invalid queries become alerts.
    let resp = get(&env.router, "/.api/search/stream?q=foo+type:nope", &[]).await;
    let events = sse_events(&resp.text());
    assert_eq!(events[0].0, "alert");
    assert_eq!(events.last().unwrap().0, "done");
    // So does a grouped `or` among the terms of a standard query.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=patterntype:standard+a+%28b+or+c%29+d",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    assert_eq!(events[0].0, "alert");
    let alert: serde_json::Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(alert["title"], "Unable To Process Query");
    assert!(
        alert["description"]
            .as_str()
            .unwrap()
            .contains("trouble understanding that query")
    );
    // And a `)` that closes no group.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=patterntype:standard+x%28%29%28y+or+z%29",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    assert_eq!(events[0].0, "alert");
    let alert: serde_json::Value = serde_json::from_str(&events[0].1).unwrap();
    assert!(
        alert["description"]
            .as_str()
            .unwrap()
            .starts_with("unsupported expression")
    );

    // Symbol search: the definitions whose names match.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=repo:demo%24+type:symbol+main",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "matches").unwrap();
    let m: Vec<serde_json::Value> = serde_json::from_str(data).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "symbol");
    assert_eq!(m[0]["path"], "src/main.rs");
    assert_eq!(m[0]["language"], "Rust");
    let sym = &m[0]["symbols"][0];
    assert_eq!(sym["name"], "main");
    assert_eq!(sym["kind"], "FUNCTION");
    assert_eq!(sym["containerName"], "");
    assert_eq!(sym["line"], 1);
    assert!(
        sym["url"]
            .as_str()
            .unwrap()
            .ends_with("/-/blob/src/main.rs#L1:4-1:8"),
        "{sym}"
    );

    // `select:` projects results, each projection once.
    let (m, _) = search(&env.router, "repo:demo%24+select:repo+main").await;
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["type"], "repo");
    assert_eq!(m[0]["repository"], REPO);
    let (m, _) = search(&env.router, "repo:demo%24+select:file.directory+lang:rust").await;
    let dirs: Vec<&str> = m.iter().map(|m| m["path"].as_str().unwrap()).collect();
    assert_eq!(dirs, ["src/"]);
    assert_eq!(m[0]["type"], "path");
    let (m, _) = search(&env.router, "repo:demo%24+select:symbol.function+main").await;
    assert!(!m.is_empty());
    assert!(m.iter().all(|m| {
        m["type"] == "symbol"
            && m["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["kind"] == "FUNCTION")
    }));

    for (select, sign) in [("added", '+'), ("removed", '-')] {
        let q = format!("repo:demo%24+select:commit.diff.{select}+world");
        let (m, _) = search(&env.router, &q).await;
        for c in &m {
            assert_eq!(c["type"], "commit");
            let content = c["content"].as_str().unwrap();
            // Fence, file line, then hunk headers and kept lines only.
            for line in content
                .lines()
                .skip(2)
                .filter(|l| !l.starts_with("@@") && *l != "```")
            {
                assert!(line.starts_with(sign), "{select}: {content}");
            }
        }
        if select == "added" {
            assert!(!m.is_empty());
        }
    }

    // Unsupported parts are named in an alert.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=repo:demo%24+type:diff+file:has.owner(alice)+main",
        &[],
    )
    .await;
    let events = sse_events(&resp.text());
    let (_, data) = events.iter().find(|(e, _)| e == "alert").unwrap();
    let a: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(a["title"], "Unsupported Query");
    assert_eq!(
        a["description"],
        "`file:has.owner()` in commit and diff searches is not supported yet."
    );
    assert!(!events.iter().any(|(e, _)| e == "matches"));

    let denied = get(&env.private_router, "/.api/search/stream?q=x", &[]).await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn searches_are_usage_events() {
    let Some(env) = setup().await else { return };
    // A signed-out visitor (anonymous access is on) with the web app's cookie.
    let resp = get(
        &env.router,
        "/.api/search/stream?q=hello&t=regexp",
        &[("cookie", "theme=dark; sourcegullAnonymousUid=visitor-1")],
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    // Recorded right after the last event.
    let mut row = None;
    for _ in 0..50 {
        row = sqlx::query_as::<_, (Option<i64>, String, String, serde_json::Value)>(
            "SELECT user_id, anonymous_user_id, source, argument FROM event_logs \
             WHERE name = 'SearchResultsQueried'",
        )
        .fetch_optional(&env._t.pool)
        .await
        .unwrap();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (user, anon, source, arg) = row.expect("a search usage event");
    assert_eq!(
        (user, anon.as_str(), source.as_str()),
        (None, "visitor-1", "BACKEND")
    );
    assert_eq!(arg["type"], "regexp");
    assert_eq!(arg["patternType"], "regexp");
    assert_eq!(arg["resultCount"], 2);
    assert!(arg["durationMs"].is_u64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_stream_defaults_to_compatible_line_matches() {
    let Some(env) = setup().await else { return };
    let resp = get(&env.router, "/.api/search/stream?q=hello", &[]).await;
    assert_eq!(resp.status, StatusCode::OK);
    let events = sse_events(&resp.text());
    let rows: Vec<serde_json::Value> = events
        .iter()
        .filter(|(event, _)| event == "matches")
        .flat_map(|(_, data)| serde_json::from_str::<Vec<serde_json::Value>>(data).unwrap())
        .collect();
    let main = rows
        .iter()
        .find(|row| row["path"] == "src/main.rs")
        .unwrap();
    assert!(main.get("chunkMatches").is_none());
    assert_eq!(
        main["lineMatches"][0]["line"],
        "    println!(\"hello, world\");"
    );
    assert_eq!(
        main["lineMatches"][0]["offsetAndLengths"],
        serde_json::json!([[14, 5]])
    );
    assert_eq!(events.last().unwrap().0, "done");
}

/// The matches and the final progress of a search.
async fn search(router: &Router, q: &str) -> (Vec<serde_json::Value>, serde_json::Value) {
    let resp = get(router, &format!("/.api/search/stream?q={q}&cm=t"), &[]).await;
    assert_eq!(resp.status, StatusCode::OK);
    let events = sse_events(&resp.text());
    assert_eq!(events.last().unwrap().0, "done");
    let matches = events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<serde_json::Value>>(d).unwrap())
        .collect();
    let (_, progress) = events.iter().rfind(|(e, _)| e == "progress").unwrap();
    (matches, serde_json::from_str(progress).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_uses_the_index() {
    let Some(env) = setup().await else { return };
    // Nothing indexed yet: `index:only` finds nothing, the default falls back.
    let (m, _) = search(&env.router, "hello+index:only").await;
    assert!(m.is_empty());

    env.node.index_repo(env.id, REPO, &["feature"]);
    let (m, progress) = search(&env.router, "hello+index:only").await;
    let mut paths: Vec<&str> = m.iter().map(|m| m["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["README.md", "src/main.rs"]);
    let main = m.iter().find(|m| m["path"] == "src/main.rs").unwrap();
    assert_eq!(main["commit"], env.oids.c3);
    assert!(main["branches"].is_null());
    assert_eq!(main["language"], "Rust");
    assert_eq!(
        main["chunkMatches"][0]["content"],
        "    println!(\"hello, world\");"
    );
    assert_eq!(main["chunkMatches"][0]["ranges"][0]["start"]["column"], 14);
    // The uncloned repo is not indexed, so `index:only` leaves it out.
    assert_eq!(progress["skipped"], serde_json::json!([]));

    // The default searches the index and falls back for the rest.
    let (m, progress) = search(&env.router, "hello").await;
    assert_eq!(m.len(), 2);
    assert_eq!(progress["skipped"][0]["reason"], "repository-missing");

    // Indexed and unindexed search agree, including on other branches.
    for q in [
        "fn+repo:demo%24+rev:feature+lang:rust",
        "type:path+main",
        "file:%5Esrc/+world",
        "fn%5Cs%2Bmain+patterntype:regexp+case:yes",
        "type:symbol+main",
        "type:symbol+repo:demo%24+rev:feature",
    ] {
        let (mut indexed, _) = search(&env.router, &format!("{q}+index:only")).await;
        let (mut unindexed, _) = search(&env.router, &format!("{q}+index:no")).await;
        let key = |m: &serde_json::Value| m["path"].as_str().unwrap().to_owned();
        indexed.sort_by_key(key);
        unindexed.sort_by_key(key);
        assert!(!indexed.is_empty(), "{q}");
        assert_eq!(indexed, unindexed, "{q}");
    }
    let (m, _) = search(&env.router, "fn+repo:demo%24+rev:feature+index:only").await;
    assert_eq!(m[0]["commit"], env.oids.f1);
    assert_eq!(m[0]["branches"][0], "feature");
    // Structural search on indexed candidates (it never runs with index:no).
    let (m, _) = search(
        &env.router,
        "fn+:%5B%5Bname%5D%5D()+patterntype:structural+index:only",
    )
    .await;
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["path"], "src/main.rs");
    assert_eq!(m[0]["chunkMatches"][0]["content"], "fn main() {");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_contexts_scope_repositories() {
    use sourcegull_db::{SearchContextInput, stores::Namespace};
    let Some(env) = setup().await else { return };
    let db = Db::from_pool(env._t.pool.clone());
    let uncloned = db.repos().get_by_name(UNCLONED).await.unwrap().unwrap().id;
    let contexts = db.search_contexts();
    let context =
        |name: &str, public: bool, query: Option<&str>, repos: Vec<(RepoId, Vec<String>)>| {
            SearchContextInput {
                name: name.into(),
                public,
                query: query.map(str::to_owned),
                repos,
                ..Default::default()
            }
        };
    contexts
        .create(
            None,
            &context(
                "feature",
                true,
                None,
                vec![(env.id, vec!["feature".into()])],
            ),
        )
        .await
        .unwrap();
    contexts
        .create(
            None,
            &context("pending", true, None, vec![(uncloned, vec![])]),
        )
        .await
        .unwrap();
    contexts
        .create(
            None,
            &context("demo", true, Some("repo:demo$ file:^src/"), vec![]),
        )
        .await
        .unwrap();
    contexts
        .create(
            None,
            &context("hidden", false, None, vec![(env.id, vec![])]),
        )
        .await
        .unwrap();
    let user = db
        .users()
        .create(&sourcegull_db::NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    contexts
        .create(
            Some(Namespace::User(user)),
            &context("mine", true, None, vec![(env.id, vec![])]),
        )
        .await
        .unwrap();

    // A listed context: its repositories, on its revisions.
    let (m, progress) = search(&env.router, "context:feature+fn+lang:rust").await;
    assert_eq!(progress["repositoriesCount"], 1);
    assert!(!m.is_empty());
    assert!(m.iter().all(|m| m["commit"] == env.oids.f1), "{m:?}");
    let (m, progress) = search(&env.router, "context:pending+hello").await;
    assert!(m.is_empty());
    assert_eq!(progress["skipped"][0]["reason"], "repository-missing");
    // The query's filters still apply within the context.
    let (m, progress) = search(&env.router, "context:feature+repo:uncloned+hello").await;
    assert!(m.is_empty());
    assert_eq!(progress["repositoriesCount"], 0);
    let (m, _) = search(&env.router, "context:%40alice/mine+hello").await;
    assert_eq!(m.len(), 2);

    // A query-defined context: its query replaces the term.
    let (m, progress) = search(&env.router, "context:demo+hello").await;
    assert_eq!(progress["repositoriesCount"], 1);
    let paths: Vec<&str> = m.iter().map(|m| m["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["src/main.rs"]);

    // `global` is every repository; unknown and private contexts are alerts.
    let (m, progress) = search(&env.router, "context:global+hello").await;
    assert_eq!(
        (m.len(), progress["repositoriesCount"].as_u64()),
        (2, Some(2))
    );
    for q in [
        "context:nope+hello",
        "context:hidden+hello",
        "context:%40alice+hello",
    ] {
        let resp = get(&env.router, &format!("/.api/search/stream?q={q}"), &[]).await;
        let events = sse_events(&resp.text());
        let (_, alert) = events.iter().find(|(e, _)| e == "alert").expect(q);
        let alert: serde_json::Value = serde_json::from_str(alert).unwrap();
        assert_eq!(alert["title"], "Search context not found", "{q}");
        assert!(!events.iter().any(|(e, _)| e == "matches"), "{q}");
    }
}

/// Search and file reads as `token` (anonymous when `None`).
async fn as_user(
    router: &Router,
    token: Option<&str>,
    q: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let auth = token.map(|t| format!("token {t}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
    let resp = get(router, &format!("/.api/search/stream?q={q}&cm=t"), &headers).await;
    assert_eq!(resp.status, StatusCode::OK);
    let events = sse_events(&resp.text());
    let matches = events
        .iter()
        .filter(|(e, _)| e == "matches")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<serde_json::Value>>(d).unwrap())
        .collect();
    let (_, progress) = events.iter().rfind(|(e, _)| e == "progress").unwrap();
    (matches, serde_json::from_str(progress).unwrap())
}

/// The events (but timings) of `token`'s search for `hello` in the repository `name` at a
/// revision that does not exist, with `name` replaced by `<repo>`.
async fn search_events(router: &Router, token: Option<&str>, name: &str) -> Vec<(String, String)> {
    let auth = token.map(|t| format!("token {t}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
    let q = format!("repo:%5E{}%24@no-such-rev+hello", name.replace('/', "%2F"));
    let resp = get(router, &format!("/.api/search/stream?q={q}&cm=t"), &headers).await;
    assert_eq!(resp.status, StatusCode::OK);
    sse_events(&resp.text().replace(name, "<repo>"))
        .into_iter()
        .map(|(e, d)| {
            if e != "progress" {
                return (e, d);
            }
            let mut p: serde_json::Value = serde_json::from_str(&d).unwrap();
            p.as_object_mut().unwrap().remove("durationMs");
            (e, p.to_string())
        })
        .collect()
}

/// Whether `token`'s search for `hello` finds the demo repository.
async fn sees_demo(router: &Router, token: Option<&str>) -> bool {
    let (m, _) = as_user(router, token, "hello").await;
    m.iter().any(|m| m["repository"] == REPO)
}

/// Polls until `sees_demo` is `want` (grants apply on `NOTIFY authz`, well before the cache TTL).
async fn wait_sees_demo(router: &Router, token: Option<&str>, want: bool) {
    for _ in 0..50 {
        if sees_demo(router, token).await == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("sees_demo never became {want}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_hide_private_repositories() {
    hide_private_repositories("gitlab", "https://gitlab.example.com/").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_hide_private_github_repositories() {
    hide_private_repositories("github", "https://github.com/").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_hide_private_gitea_repositories() {
    hide_private_repositories("gitea", "https://gitea.example.com/").await;
}

/// Every read path hides a private repository of the code host `service_type` at
/// `service_id` (enforcement does not depend on the code host).
async fn hide_private_repositories(service_type: &str, service_id: &str) {
    use sourcegull_authn::AuthVia;
    use sourcegull_db::PermsSource;
    let Some(env) = setup().await else { return };
    let db = Db::from_pool(env._t.pool.clone());
    sqlx::query(
        "UPDATE repo SET private = true, external_service_type = $2, external_service_id = $3
         WHERE id = $1",
    )
    .bind(env.id)
    .bind(service_type)
    .bind(service_id)
    .execute(db.pool())
    .await
    .unwrap();
    let state = state(&db, true, &env.node);
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(state.authz.clone().run_listener(cancel.clone()));
    let router = public_router(state.clone());
    let auth = &state.auth;
    let admin = auth.sign_up("admin", "admin@example.com", "correct horse battery");
    let admin = admin.await.unwrap().id;
    let alice = sourcegull_db::NewUser {
        username: "alice".into(),
        ..Default::default()
    };
    let alice = db.users().create(&alice).await.unwrap().id;
    let mut tokens = Vec::new();
    for user in [admin, alice] {
        let actor = auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let (_, secret) = auth
            .create_access_token(&actor, user, &["user:all".into()], "t", None)
            .await
            .unwrap();
        tokens.push(secret);
    }
    let (admin_token, alice_token) = (Some(tokens[0].as_str()), Some(tokens[1].as_str()));

    // A private repository nobody was granted: site admins only.
    assert!(sees_demo(&router, admin_token).await);
    // (For a viewer who can read it, the bad revision shows: the parity checks below mean
    // something.)
    assert_ne!(
        search_events(&router, admin_token, REPO).await,
        search_events(&router, admin_token, "gitlab.example.com/acme/nope").await
    );
    for token in [None, alice_token] {
        let (m, progress) = as_user(&router, token, "hello").await;
        assert!(m.iter().all(|m| m["repository"] != REPO), "{m:?}");
        // Not searched or counted: only the public uncloned repository is.
        assert_eq!(progress["repositoriesCount"], 1);
        let (m, _) = as_user(&router, token, "type:repo+demo").await;
        assert!(m.is_empty(), "{m:?}");
        // File reads answer exactly like a missing repository.
        let auth = token.map(|t| format!("token {t}"));
        let headers: Vec<(&str, &str)> =
            auth.iter().map(|a| ("authorization", a.as_str())).collect();
        let hidden = get(&router, &format!("/{REPO}/-/raw/README.md"), &headers).await;
        let missing = get(
            &router,
            "/gitlab.example.com/acme/nope/-/raw/README.md",
            &headers,
        )
        .await;
        assert_eq!(hidden.status, StatusCode::NOT_FOUND);
        assert_eq!((hidden.status, hidden.body), (missing.status, missing.body));
        let blame_uri = format!("/.api/blame/{REPO}/-/stream/src/main.rs");
        let blame = get(&router, &blame_uri, &headers).await;
        let blame_missing = get(
            &router,
            "/.api/blame/gitlab.example.com/acme/nope/-/stream/src/main.rs",
            &headers,
        )
        .await;
        assert_eq!(blame.status, StatusCode::NOT_FOUND);
        assert_eq!(
            (blame.status, blame.body),
            (blame_missing.status, blame_missing.body)
        );
        // Directory archives too.
        let archive = get(&router, &format!("/{REPO}/-/raw/src?format=zip"), &headers).await;
        let archive_missing = get(
            &router,
            "/gitlab.example.com/acme/nope/-/raw/src?format=zip",
            &headers,
        )
        .await;
        assert_eq!(
            (archive.status, archive.body),
            (archive_missing.status, archive_missing.body)
        );
        // A search naming the repository (with a bad revision, which would fail only for a
        // repository that exists) answers exactly like one naming a missing repository.
        let hidden = search_events(&router, token, REPO).await;
        let missing = search_events(&router, token, "gitlab.example.com/acme/nope").await;
        assert_eq!(hidden, missing);
    }

    // A grant applies at once, a revocation too.
    db.perms()
        .set_repo_perms(env.id, PermsSource::Explicit, &[alice], None)
        .await
        .unwrap();
    wait_sees_demo(&router, alice_token, true).await;
    let hello = get(
        &router,
        &format!("/{REPO}/-/raw/README.md"),
        &[("authorization", &format!("token {}", tokens[1]))],
    )
    .await;
    assert_eq!(hello.status, StatusCode::OK);
    assert!(!sees_demo(&router, None).await);
    db.perms()
        .set_repo_perms(env.id, PermsSource::Explicit, &[], None)
        .await
        .unwrap();
    wait_sees_demo(&router, alice_token, false).await;

    // Unrestricted: everyone, within the unrestricted set's TTL.
    db.perms().set_unrestricted(&[env.id], true).await.unwrap();
    wait_sees_demo(&router, None, true).await;
    cancel.cancel();
}

/// The events (but timings) of `token`'s search for `hello` in `context`.
async fn context_events(router: &Router, token: Option<&str>, context: &str) -> Vec<String> {
    let auth = token.map(|t| format!("token {t}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("authorization", a.as_str())).collect();
    let uri = format!("/.api/search/stream?q=context:{context}+hello&cm=t");
    let resp = get(router, &uri, &headers).await;
    assert_eq!(resp.status, StatusCode::OK);
    sse_events(&resp.text())
        .into_iter()
        .map(|(e, d)| {
            if e != "progress" {
                return format!("{e} {d}");
            }
            let mut p: serde_json::Value = serde_json::from_str(&d).unwrap();
            p.as_object_mut().unwrap().remove("durationMs");
            format!("{e} {p}")
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_contexts_hide_private_repositories() {
    use sourcegull_authn::AuthVia;
    use sourcegull_db::SearchContextInput;
    let Some(env) = setup().await else { return };
    let db = Db::from_pool(env._t.pool.clone());
    sqlx::query("UPDATE repo SET private = true WHERE id = $1")
        .bind(env.id)
        .execute(db.pool())
        .await
        .unwrap();
    let uncloned = db.repos().get_by_name(UNCLONED).await.unwrap().unwrap().id;
    let contexts = db.search_contexts();
    for (name, repos) in [
        // The private repository next to a public one.
        ("both", vec![(env.id, vec![]), (uncloned, vec![])]),
        // Only the private repository, at a revision that does not exist (which would fail
        // only for a repository the viewer can read).
        ("hidden", vec![(env.id, vec!["no-such-rev".to_owned()])]),
        // What a viewer who cannot read it must see instead: nothing.
        ("empty", vec![]),
    ] {
        let input = SearchContextInput {
            name: name.into(),
            public: true,
            repos,
            ..Default::default()
        };
        contexts.create(None, &input).await.unwrap();
    }
    let state = state(&db, true, &env.node);
    let router = public_router(state.clone());
    let auth = &state.auth;
    let admin = auth.sign_up("admin", "admin@example.com", "correct horse battery");
    let admin = admin.await.unwrap().id;
    let alice = sourcegull_db::NewUser {
        username: "alice".into(),
        ..Default::default()
    };
    let alice = db.users().create(&alice).await.unwrap().id;
    let mut tokens = Vec::new();
    for user in [admin, alice] {
        let actor = auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let (_, secret) = auth
            .create_access_token(&actor, user, &["user:all".into()], "t", None)
            .await
            .unwrap();
        tokens.push(secret);
    }
    let (admin_token, alice_token) = (Some(tokens[0].as_str()), Some(tokens[1].as_str()));

    // A site admin searches the private repository, and sees its bad revision (so the parity
    // check below means something).
    let (m, progress) = as_user(&router, admin_token, "context:both+hello").await;
    assert!(m.iter().any(|m| m["repository"] == REPO), "{m:?}");
    assert_eq!(progress["repositoriesCount"], 2);
    assert_ne!(
        context_events(&router, admin_token, "hidden").await,
        context_events(&router, admin_token, "empty").await
    );
    for token in [None, alice_token] {
        // Not searched, counted or skipped: only the public uncloned repository is.
        let (m, progress) = as_user(&router, token, "context:both+hello").await;
        assert!(m.is_empty(), "{m:?}");
        assert_eq!(progress["repositoriesCount"], 1);
        let reasons: Vec<&serde_json::Value> = progress["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| &s["reason"])
            .collect();
        assert_eq!(reasons, ["repository-missing"]);
        let skipped = progress["skipped"].to_string();
        assert!(!skipped.contains(REPO), "{skipped}");
        // A context of only the private repository answers like an empty one.
        assert_eq!(
            context_events(&router, token, "hidden").await,
            context_events(&router, token, "empty").await
        );
    }
}

/// `/.api/compute/stream`: the `results` of every event, flattened.
async fn compute(router: &Router, q: &str) -> (Vec<serde_json::Value>, Vec<(String, String)>) {
    let uri = format!(
        "/.api/compute/stream?q={}",
        percent_encoding::utf8_percent_encode(q, percent_encoding::NON_ALPHANUMERIC)
    );
    let resp = get(router, &uri, &[]).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let events = sse_events(&resp.text());
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    let results = events
        .iter()
        .filter(|(e, _)| e == "results")
        .flat_map(|(_, d)| serde_json::from_str::<Vec<serde_json::Value>>(d).unwrap())
        .collect();
    (results, events)
}

fn values(results: &[serde_json::Value]) -> Vec<String> {
    let mut v: Vec<String> = results
        .iter()
        .map(|r| r["value"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_streams_results() {
    let Some(env) = setup().await else { return };

    // Match only: capture groups with their ranges.
    let (r, events) = compute(&env.router, r#"repo:demo$ println!\("(\w+)"#).await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["path"], "src/main.rs");
    assert_eq!(r[0]["repository"], REPO);
    assert_eq!(r[0]["repositoryID"], env.id.get());
    let m = &r[0]["matches"][0];
    assert_eq!(m["value"], "println!(\"hello");
    assert_eq!(m["environment"]["1"]["value"], "hello");
    assert_eq!(m["environment"]["1"]["range"]["start"]["line"], 1);
    assert_eq!(m["environment"]["1"]["range"]["start"]["column"], 14);
    let (_, progress) = events.iter().rfind(|(e, _)| e == "progress").unwrap();
    assert!(progress.contains("\"done\":true"), "{progress}");

    // Output: captures and metavariables, one text per file.
    let (r, _) = compute(
        &env.router,
        r"repo:demo$ content:output(fn (\w+)\( -> $1 in $path ($lang))",
    )
    .await;
    assert_eq!(
        values(&r),
        ["add in src/lib.rs (Rust)\n", "main in src/main.rs (Rust)\n"]
    );
    assert!(
        r.iter()
            .all(|t| t["kind"] == "output" && t.get("repository").is_none())
    );

    // output.extra carries the repository; separator: sets the separator.
    let (r, _) = compute(
        &env.router,
        r"repo:demo$ file:\.rs$ content:output.extra(\b(\w)\b -> $1) separator:,",
    )
    .await;
    let lib = r.iter().find(|t| t["value"] == "a,b,a,b,").expect("lib.rs");
    assert_eq!(lib["repository"], REPO);

    // Replace rewrites the whole file.
    let (r, _) = compute(&env.router, "repo:demo$ content:replace(hello -> bye)").await;
    let main = r
        .iter()
        .find(|t| t["value"].as_str().unwrap().starts_with("fn main"))
        .expect("main.rs");
    assert_eq!(
        main["value"],
        "fn main() {\n    println!(\"bye, world\");\n    run();\n}\n"
    );
    assert_eq!(main["kind"], "replace-in-place");
    assert_eq!(main["repository"], REPO);
    assert_eq!(main["path"], "src/main.rs");
    assert_eq!(main["commit"].as_str().map(str::len), Some(40));

    // Structural output through sourcegull-structural.
    let (r, _) = compute(
        &env.router,
        "repo:demo$ content:output.structural(println!(:[args]) -> :[args])",
    )
    .await;
    assert_eq!(values(&r), ["\"hello, world\"\n"]);

    // Commit results: the message.
    let (r, _) = compute(
        &env.router,
        r"repo:demo$ type:commit content:output(c(\d): (\w+) -> $2@$1)",
    )
    .await;
    assert_eq!(values(&r), ["docs@3\n", "greet@2\n", "initial@1\n"]);

    // Invalid queries are rejected; private instances need a viewer.
    let bad = get(&env.router, "/.api/compute/stream?q=a+or+b", &[]).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.text().contains("single pattern or command"));
    let empty = get(&env.router, "/.api/compute/stream?q=", &[]).await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    let private = get(&env.private_router, "/.api/compute/stream?q=hello", &[]).await;
    assert_eq!(private.status, StatusCode::UNAUTHORIZED);
}
