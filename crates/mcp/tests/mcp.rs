//! The MCP endpoint end to end, against the test Postgres and an in-process repo
//! node, through the HTTP server: transport and auth rules, every tool, and repository
//! permissions (a private repository looks missing to a user without access).

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const DEMO: &str = "gitlab.example.com/acme/demo";
const SECRET: &str = "gitlab.example.com/acme/secret";

struct Env {
    _t: TestDb,
    _node: RepoNode,
    _sources: Vec<SourceRepo>,
    url: String,
    http: reqwest::Client,
    /// Raw access tokens.
    alice: String,
    admin: String,
    main: String,
    dev: String,
    _cancel: tokio_util::sync::DropGuard,
}

async fn user(db: &Db, auth: &Authenticator, name: &str, admin: bool) -> String {
    let id = db
        .users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    if admin {
        sourcegull_db::sqlx::query("UPDATE users SET site_admin = true WHERE id = $1")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let actor = auth.actor_for(id, AuthVia::Session).await.unwrap().unwrap();
    let (_, token) = auth
        .create_access_token(&actor, id, &["user:all".into()], "t", None)
        .await
        .unwrap();
    token
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let demo = SourceRepo::new();
    let main = demo.commit(
        "Add the server",
        &[
            ("README.md", "demo\n"),
            (
                "src/server.go",
                "package demo\n\n// NewServer creates a server.\nfunc NewServer() *Server {\n\treturn &Server{}\n}\n\ntype Server struct{}\n",
            ),
            (
                "src/main.go",
                "package demo\n\nfunc main() {\n\ts := NewServer()\n\t_ = s // TODO: run it\n}\n",
            ),
            ("docs/guide.md", "# Guide\n"),
        ],
    );
    demo.git(&["checkout", "-q", "-b", "dev"]);
    let dev = demo.commit(
        "Start the server",
        &[(
            "src/main.go",
            "package demo\n\nfunc main() {\n\ts := NewServer()\n\ts.Start()\n}\n",
        )],
    );
    demo.git(&["checkout", "-q", "main"]);
    let secret = SourceRepo::new();
    secret.commit("init", &[("main.go", "// TODO secret NewServer\n")]);
    let remotes: std::collections::HashMap<String, _> = [
        (DEMO.to_owned(), demo.remote()),
        (SECRET.to_owned(), secret.remote()),
    ]
    .into();
    let node = RepoNode::start_with(move |r| Ok(remotes[&r.name].clone())).await;
    for (i, name) in [DEMO, SECRET].into_iter().enumerate() {
        let id: i32 = sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id, \
                 private, description) \
             VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/', $3, 'A demo') RETURNING id",
        )
        .bind(name)
        .bind(i.to_string())
        .bind(name == SECRET)
        .fetch_one(db.pool())
        .await
        .unwrap();
        node.clone_repo(RepoId(id), name).await;
    }

    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.example.com".into()),
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let alice = user(&db, &auth, "alice", false).await;
    let admin = user(&db, &auth, "admin", true).await;

    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone());
    let routes = sourcegull_http_api::public_routes().merge(sourcegull_mcp::routes());
    let router = sourcegull_http_api::finish(routes, state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        sourcegull_mcp::PATH
    );
    let cancel = CancellationToken::new();
    tokio::spawn(sourcegull_http_api::serve(listener, router, cancel.clone()));

    Some(Env {
        _t: t,
        _node: node,
        _sources: vec![demo, secret],
        url,
        http: reqwest::Client::new(),
        alice,
        admin,
        main,
        dev,
        _cancel: cancel.drop_guard(),
    })
}

impl Env {
    async fn post(&self, token: &str, body: Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let resp = self
            .post(
                token,
                json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }),
            )
            .await;
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["id"], 7);
        v
    }

    /// Calls a tool; returns (text, isError).
    async fn tool(&self, token: &str, name: &str, args: Value) -> (String, bool) {
        let v = self
            .rpc(
                token,
                "tools/call",
                json!({ "name": name, "arguments": args }),
            )
            .await;
        let r = &v["result"];
        assert!(r.is_object(), "{name}: {v}");
        (
            r["content"][0]["text"].as_str().unwrap().to_owned(),
            r["isError"].as_bool().unwrap(),
        )
    }

    async fn ok(&self, name: &str, args: Value) -> String {
        let (text, err) = self.tool(&self.alice, name, args.clone()).await;
        assert!(!err, "{name} {args}: {text}");
        text
    }
}

#[tokio::test]
async fn transport_and_auth() {
    let Some(env) = setup().await else { return };

    let v = env
        .rpc(
            &env.alice,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" },
            }),
        )
        .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(v["result"]["serverInfo"]["name"], "sourcegull");
    assert!(v["result"]["capabilities"]["tools"].is_object());

    let resp = env
        .post(
            &env.alice,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
    assert_eq!(resp.status(), 202);

    let v = env.rpc(&env.alice, "tools/list", json!({})).await;
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in [
        "search",
        "read_file",
        "list_files",
        "find_symbols",
        "go_to_definition",
        "find_references",
        "find_implementations",
        "list_repos",
        "list_branches",
        "commit_log",
        "get_commit",
        "compare_revisions",
        "blame",
    ] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    for t in v["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert_eq!(t["annotations"]["readOnlyHint"], true);
    }

    let v = env.rpc(&env.alice, "nope", json!({})).await;
    assert_eq!(v["error"]["code"], -32601);
    let v = env
        .rpc(&env.alice, "tools/call", json!({ "name": "nope" }))
        .await;
    assert_eq!(v["error"]["code"], -32602);
    let v = env.rpc(&env.alice, "ping", json!({})).await;
    assert_eq!(v["result"], json!({}));

    // A batch answers every request and skips notifications.
    let resp = env
        .post(
            &env.alice,
            json!([
                { "jsonrpc": "2.0", "id": 1, "method": "ping" },
                { "jsonrpc": "2.0", "method": "notifications/initialized" },
                { "jsonrpc": "2.0", "id": 2, "method": "ping" },
            ]),
        )
        .await;
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);

    // Parse errors are JSON-RPC errors.
    let resp = env
        .http
        .post(&env.url)
        .bearer_auth(&env.alice)
        .body("{")
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], -32700);

    // `token` works as well as Bearer; no credentials or a bad token do not.
    let resp = env
        .http
        .post(&env.url)
        .header("authorization", format!("token {}", env.alice))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = env
        .http
        .post(&env.url)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(resp.headers().contains_key("www-authenticate"));
    let resp = env
        .post(
            "skp_bogus",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
        )
        .await;
    assert_eq!(resp.status(), 401);

    // Cross-origin browser requests are refused; the instance's own origin is fine.
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
    let resp = env
        .http
        .post(&env.url)
        .bearer_auth(&env.alice)
        .header("origin", "https://evil.example.com")
        .json(&ping)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let resp = env
        .http
        .post(&env.url)
        .bearer_auth(&env.alice)
        .header("origin", "https://sourcegull.example.com")
        .json(&ping)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = env.http.get(&env.url).send().await.unwrap();
    assert_eq!(resp.status(), 405);
}

#[tokio::test]
async fn admin_instructions_point_to_the_api_or_the_files() {
    let Some(env) = setup().await else { return };
    let instructions = async |token: &str| {
        let v = env
            .rpc(
                token,
                "initialize",
                json!({ "protocolVersion": "2025-06-18" }),
            )
            .await;
        v["result"]["instructions"].as_str().unwrap().to_owned()
    };

    let text = instructions(&env.alice).await;
    assert!(text.contains("find_symbols"));
    assert!(!text.contains("site admin"), "{text}");

    let text = instructions(&env.admin).await;
    assert!(text.contains("find_symbols"));
    assert!(text.contains("site admin"), "{text}");
    assert!(text.contains("/.api/graphql"), "{text}");
    assert!(text.contains("SOURCEGULL_ADMIN_TOKEN"), "{text}");
    assert!(text.contains("updateSiteConfiguration"), "{text}");
    assert!(text.contains("overwriteSettings"), "{text}");

    // Global settings from a file: the agent is sent to the file, not the mutation.
    Db::from_pool(env._t.pool.clone())
        .declared()
        .record_applied(
            sourcegull_db::GLOBAL_SETTINGS_SOURCE,
            "/etc/sourcegull/global-settings.jsonc",
            &[],
            false,
            b"x",
        )
        .await
        .unwrap();
    let text = instructions(&env.admin).await;
    assert!(
        text.contains("SOURCEGULL_GLOBAL_SETTINGS_FILE, `/etc/sourcegull/global-settings.jsonc`"),
        "{text}"
    );
    assert!(!text.contains("overwriteSettings"), "{text}");
    assert!(text.contains("updateSiteConfiguration"), "{text}");
}

#[tokio::test]
async fn search_and_files() {
    let Some(env) = setup().await else { return };

    let text = env.ok("search", json!({ "query": "TODO" })).await;
    assert!(text.contains(&format!("{DEMO} src/main.go")), "{text}");
    assert!(text.contains("5:\t_ = s // TODO: run it"), "{text}");
    assert!(
        !text.contains("secret"),
        "alice must not see the secret repo: {text}"
    );
    let text = env
        .ok(
            "search",
            json!({ "query": "NewServer", "repo": "acme/demo", "limit": 1 }),
        )
        .await;
    assert!(text.contains("Result limit 1 reached"), "{text}");
    let text = env
        .ok("search", json!({ "query": "repo:demo file:server /e/", "pattern_type": "standard", "max_lines_per_file": 2 }))
        .await;
    assert!(text.contains("more lines in this file"), "{text}");
    let (text, err) = env
        .tool(
            &env.alice,
            "search",
            json!({ "query": "select:nonsense foo" }),
        )
        .await;
    assert!(err, "{text}");
    let text = env
        .ok("search", json!({ "query": "type:commit server" }))
        .await;
    assert!(text.contains("Add the server"), "{text}");

    // The admin sees the secret repository.
    let (text, _) = env
        .tool(&env.admin, "search", json!({ "query": "TODO secret" }))
        .await;
    assert!(text.contains(SECRET), "{text}");

    let text = env
        .ok(
            "read_file",
            json!({ "repo": "acme/demo", "path": "src/server.go", "start_line": 3, "end_line": 4 }),
        )
        .await;
    assert!(text.contains("(lines 3-4 of 8)"), "{text}");
    assert!(text.contains("3\t// NewServer creates a server."), "{text}");
    assert!(text.contains("4\tfunc NewServer() *Server {"), "{text}");
    assert!(
        !text.contains("start_line="),
        "an explicit range needs no continuation: {text}"
    );
    let text = env
        .ok(
            "read_file",
            json!({ "repo": DEMO, "path": "src/main.go", "rev": "dev" }),
        )
        .await;
    assert!(text.contains("s.Start()"), "{text}");
    let text = env
        .ok("read_file", json!({ "repo": DEMO, "path": "src" }))
        .await;
    assert!(text.contains("src/server.go"), "{text}");

    let (text, err) = env
        .tool(
            &env.alice,
            "read_file",
            json!({ "repo": "acme/secret", "path": "main.go" }),
        )
        .await;
    assert!(err && text.contains("not found"), "{text}");
    let (text, err) = env
        .tool(
            &env.alice,
            "read_file",
            json!({ "repo": DEMO, "path": "nope.go" }),
        )
        .await;
    assert!(err && text.contains("not found"), "{text}");

    let text = env.ok("list_files", json!({ "repo": "demo" })).await;
    assert!(
        text.lines().any(|l| l == "src/") && text.contains("README.md"),
        "{text}"
    );
    let text = env
        .ok(
            "list_files",
            json!({ "repo": DEMO, "recursive": true, "pattern": "*.go" }),
        )
        .await;
    assert!(
        text.contains("src/main.go") && text.contains("src/server.go"),
        "{text}"
    );
    assert!(!text.contains("README"), "{text}");

    let text = env.ok("list_repos", json!({})).await;
    assert!(text.contains(DEMO) && text.contains("A demo"), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let text = env
        .ok(
            "find_symbols",
            json!({ "name": "NewServer", "exact": true }),
        )
        .await;
    assert!(
        text.contains(&format!("function NewServer  {DEMO} src/server.go:4")),
        "{text}"
    );
    assert!(!text.contains(SECRET), "{text}");
}

/// A private repository from GitHub or Gitea is hidden like one from GitLab (enforcement does
/// not depend on the code host).
#[tokio::test]
async fn private_repositories_hidden_on_every_code_host() {
    for (service_type, service_id) in [
        ("github", "https://github.com/"),
        ("gitea", "https://gitea.example.com/"),
    ] {
        let Some(env) = setup().await else { return };
        sourcegull_db::sqlx::query(
            "UPDATE repo SET external_service_type = $2, external_service_id = $3 WHERE name = $1",
        )
        .bind(SECRET)
        .bind(service_type)
        .bind(service_id)
        .execute(&env._t.pool)
        .await
        .unwrap();
        let text = env.ok("search", json!({ "query": "TODO secret" })).await;
        assert!(!text.contains(SECRET), "{service_type}: {text}");
        let (text, err) = env
            .tool(
                &env.alice,
                "read_file",
                json!({ "repo": "acme/secret", "path": "main.go" }),
            )
            .await;
        assert!(err && text.contains("not found"), "{service_type}: {text}");
        let text = env.ok("list_repos", json!({})).await;
        assert!(!text.contains(SECRET), "{service_type}: {text}");
        let (text, _) = env
            .tool(&env.admin, "search", json!({ "query": "TODO secret" }))
            .await;
        assert!(text.contains(SECRET), "{service_type}: {text}");
    }
}

#[tokio::test]
async fn navigation_and_history() {
    let Some(env) = setup().await else { return };

    let text = env
        .ok(
            "go_to_definition",
            json!({ "repo": DEMO, "path": "src/main.go", "line": 4, "symbol": "NewServer" }),
        )
        .await;
    assert!(text.contains("src/server.go:4"), "{text}");
    assert!(text.contains("4\tfunc NewServer() *Server {"), "{text}");
    let (text, err) = env
        .tool(
            &env.alice,
            "go_to_definition",
            json!({ "repo": DEMO, "path": "src/main.go", "line": 4, "symbol": "Missing" }),
        )
        .await;
    assert!(err && text.contains("not on line 4"), "{text}");

    let text = env
        .ok(
            "find_references",
            json!({ "repo": DEMO, "path": "src/server.go", "line": 4, "symbol": "NewServer" }),
        )
        .await;
    assert!(text.contains(&format!("{DEMO} src/main.go")), "{text}");
    assert!(text.contains("4:\ts := NewServer()"), "{text}");
    assert!(text.contains("of `NewServer`"), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let text = env.ok("list_branches", json!({ "repo": DEMO })).await;
    assert!(
        text.contains("branch main (default)") && text.contains("branch dev"),
        "{text}"
    );

    let text = env
        .ok("commit_log", json!({ "repo": DEMO, "rev": "dev" }))
        .await;
    assert!(
        text.contains("Start the server") && text.contains("Add the server"),
        "{text}"
    );
    let text = env
        .ok("commit_log", json!({ "repo": DEMO, "path": "docs" }))
        .await;
    assert!(text.contains(&env.main[..12]), "{text}");

    let text = env
        .ok("get_commit", json!({ "repo": DEMO, "rev": env.dev }))
        .await;
    assert!(text.contains("Start the server"), "{text}");
    assert!(text.contains(" M src/main.go +1 -1"), "{text}");
    assert!(text.contains("+\ts.Start()"), "{text}");
    let text = env
        .ok(
            "get_commit",
            json!({ "repo": DEMO, "rev": "dev", "diff": false }),
        )
        .await;
    assert!(!text.contains("+\ts.Start()"), "{text}");

    let text = env
        .ok(
            "compare_revisions",
            json!({ "repo": DEMO, "base": "main", "head": "dev", "diff": true }),
        )
        .await;
    assert!(text.contains("1 commit"), "{text}");
    assert!(text.contains("-\t_ = s // TODO: run it"), "{text}");

    let text = env
        .ok(
            "blame",
            json!({ "repo": DEMO, "path": "src/main.go", "rev": "dev" }),
        )
        .await;
    assert!(
        text.contains("Start the server") && text.contains("Add the server"),
        "{text}"
    );
}
