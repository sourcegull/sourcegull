//! Search jobs end to end, against the test Postgres, an in-process repo node and a
//! filesystem blob store, through the HTTP server: GraphQL validation, create, list, node and
//! progress, the three queue workers, JSONL, CSV and log downloads, ref globs, the creator's
//! permissions, access rules, cancel, delete and the janitor.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_observability::MetricsHandle;
use sourcegull_searchjobs::worker::{SearchJobRunner, search_job_workers};
use sourcegull_searchjobs::{SearchJobStore, janitor};
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
    _blob_dir: tempfile::TempDir,
    db: Db,
    blobs: sourcegull_blobstore::Blobs,
    base: String,
    http: reqwest::Client,
    alice: String,
    bob: String,
    admin: String,
    demo_main: String,
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
    format!("token {token}")
}

async fn setup(workers: bool) -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let demo = SourceRepo::new();
    let demo_main = demo.commit(
        "init",
        &[
            ("README.md", "demo\n"),
            ("src/a.go", "package a\n\n// TODO: one\nfunc A() {}\n"),
            ("src/b.go", "package a\n// TODO, \"two\"\n// TODO three\n"),
        ],
    );
    demo.git(&["checkout", "-q", "-b", "dev"]);
    demo.commit("dev", &[("src/dev.go", "// TODO on dev\n")]);
    demo.git(&["checkout", "-q", "main"]);
    let secret = SourceRepo::new();
    secret.commit("init", &[("main.go", "// TODO secret\n")]);
    let remotes: std::collections::HashMap<String, _> = [
        (DEMO.to_owned(), demo.remote()),
        (SECRET.to_owned(), secret.remote()),
    ]
    .into();
    let node = RepoNode::start_with(move |r| Ok(remotes[&r.name].clone())).await;
    for (i, name) in [DEMO, SECRET].into_iter().enumerate() {
        let id: i32 = sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id, \
                 private) \
             VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/', $3) RETURNING id",
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
    let bob = user(&db, &auth, "bob", false).await;
    let admin = user(&db, &auth, "admin", true).await;

    let blob_dir = tempfile::tempdir().unwrap();
    let blobs: sourcegull_blobstore::Blobs =
        Arc::new(sourcegull_blobstore::FsBlobStore::open(blob_dir.path()).unwrap());
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone())
    .with_blobs(blobs.clone());
    let routes = sourcegull_http_api::public_routes()
        .merge(sourcegull::features::graphql_routes(&state))
        .merge(sourcegull_searchjobs::http::routes());
    let router = sourcegull_http_api::finish(routes, state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    tokio::spawn(sourcegull_http_api::serve(listener, router, cancel.clone()));

    let runner = SearchJobRunner::new(
        db.clone(),
        config.clone(),
        node.clients.clone(),
        blobs.clone(),
    );
    if workers {
        let workers = search_job_workers(runner, "test-worker").unwrap();
        tokio::spawn(workers.run(cancel.clone()));
    }

    Some(Env {
        _t: t,
        _node: node,
        _sources: vec![demo, secret],
        _blob_dir: blob_dir,
        db,
        blobs,
        base,
        http: reqwest::Client::new(),
        alice,
        bob,
        admin,
        demo_main,
        _cancel: cancel.drop_guard(),
    })
}

impl Env {
    async fn gql(&self, auth: &str, query: &str, vars: Value) -> Value {
        self.http
            .post(format!("{}/.api/graphql", self.base))
            .header("authorization", auth)
            .json(&json!({ "query": query, "variables": vars }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn ok(&self, auth: &str, query: &str, vars: Value) -> Value {
        let v = self.gql(auth, query, vars).await;
        assert!(v.get("errors").is_none(), "{query}: {v}");
        v["data"].clone()
    }

    /// The `extensions.code` of the first error.
    async fn err(&self, auth: &str, query: &str, vars: Value) -> (String, String) {
        let v = self.gql(auth, query, vars).await;
        let e = &v["errors"][0];
        assert!(!e.is_null(), "{query}: expected an error, got {v}");
        (
            e["extensions"]["code"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            e["message"].as_str().unwrap_or_default().to_owned(),
        )
    }

    async fn download(&self, auth: &str, file: &str) -> (u16, String, String) {
        let resp = self
            .http
            .get(format!("{}/.api/search/export/{file}", self.base))
            .header("authorization", auth)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        (status, ct, resp.text().await.unwrap())
    }

    async fn create(&self, auth: &str, query: &str) -> (String, i64) {
        let d = self
            .ok(
                auth,
                "mutation($q: String!) { createSearchJob(query: $q) { id query state URL logURL } }",
                json!({ "q": query }),
            )
            .await;
        let id = d["createSearchJob"]["id"].as_str().unwrap().to_owned();
        let url = d["createSearchJob"]["URL"].as_str().unwrap();
        let n: i64 = url
            .strip_prefix("https://sourcegull.example.com/.api/search/export/")
            .and_then(|s| s.strip_suffix(".jsonl"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            d["createSearchJob"]["logURL"],
            format!("https://sourcegull.example.com/.api/search/export/{n}.log")
        );
        (id, n)
    }

    /// Waits until the job is no longer queued or processing; returns the node.
    async fn wait(&self, auth: &str, id: &str) -> Value {
        for _ in 0..300 {
            let d = self
                .ok(
                    auth,
                    "query($id: ID!) { node(id: $id) { ... on SearchJob {
                        id query state createdAt startedAt finishedAt
                        creator { username }
                        repoStats { total completed failed inProgress }
                    } } }",
                    json!({ "id": id }),
                )
                .await;
            let node = d["node"].clone();
            if !matches!(node["state"].as_str(), Some("QUEUED" | "PROCESSING")) {
                return node;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("search job {id} did not finish");
    }
}

const VALIDATE: &str = "query($q: String!) { validateSearchJob(query: $q) { alwaysNil } }";

#[tokio::test]
async fn search_jobs_end_to_end() {
    let Some(env) = setup(true).await else { return };

    // Validation, as createSearchJob applies it.
    env.ok(&env.alice, VALIDATE, json!({ "q": "TODO" })).await;
    for (q, needle) in [
        ("TODO type:repo", "result types"),
        ("TODO select:repo", "select:"),
        ("(file:\\.go TODO) or (file:\\.md TODO)", "AND/OR"),
        ("TODO file:has.content(x)", "file predicates"),
        ("patterntype:regexp .*", ".*"),
        ("patterntype:structural foo(:[x])", "structural"),
    ] {
        let (code, message) = env.err(&env.alice, VALIDATE, json!({ "q": q })).await;
        assert_eq!(code, "VALIDATION", "{q}");
        assert!(message.contains(needle), "{q}: {message}");
        let (code, _) = env
            .err(
                &env.alice,
                "mutation($q: String!) { createSearchJob(query: $q) { id } }",
                json!({ "q": q }),
            )
            .await;
        assert_eq!(code, "VALIDATION", "{q}");
    }

    // A job over the default branches alice can read: demo, not the private repository.
    let (id, n) = env.create(&env.alice, "TODO").await;
    let (code, _) = env
        .err(
            &env.alice,
            "mutation($q: String!) { createSearchJob(query: $q) { id } }",
            json!({ "q": "TODO" }),
        )
        .await;
    assert_eq!(code, "ALREADY_EXISTS");
    let job = env.wait(&env.alice, &id).await;
    assert_eq!(job["state"], "COMPLETED", "{job}");
    assert_eq!(job["creator"]["username"], "alice");
    assert!(job["finishedAt"].is_string());
    assert_eq!(
        job["repoStats"],
        json!({ "total": 1, "completed": 1, "failed": 0, "inProgress": 0 })
    );

    let (status, ct, body) = env.download(&env.alice, &format!("{n}.jsonl")).await;
    assert_eq!(status, 200);
    assert_eq!(ct, "application/jsonlines");
    let lines: Vec<Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut paths: Vec<&str> = lines.iter().map(|l| l["path"].as_str().unwrap()).collect();
    paths.sort();
    assert_eq!(paths, ["src/a.go", "src/b.go"], "{body}");
    for l in &lines {
        assert_eq!(l["type"], "content");
        assert_eq!(l["repository"], DEMO);
        assert_eq!(l["commit"], env.demo_main);
        assert!(l["chunkMatches"].as_array().is_some_and(|c| !c.is_empty()));
    }

    let (status, ct, csv) = env.download(&env.alice, &format!("{n}.csv")).await;
    assert_eq!(status, 200);
    assert_eq!(ct, "text/csv");
    let mut rows: Vec<&str> = csv.lines().collect();
    assert_eq!(
        rows.remove(0),
        "repository,revision,file_path,match_count,first_match_url"
    );
    rows.sort();
    let main = &env.demo_main;
    assert_eq!(
        rows,
        [
            format!(
                "{DEMO},{main},src/a.go,1,https://sourcegull.example.com/{DEMO}@{main}/-/blob/src/a.go?L3"
            ),
            format!(
                "{DEMO},{main},src/b.go,2,https://sourcegull.example.com/{DEMO}@{main}/-/blob/src/b.go?L2"
            ),
        ]
    );

    let (status, _, log) = env.download(&env.alice, &format!("{n}.log")).await;
    assert_eq!(status, 200);
    let log: Vec<&str> = log.lines().collect();
    assert_eq!(
        log[0],
        "repository,revision,started_at,finished_at,status,failure_message"
    );
    assert_eq!(log.len(), 2);
    assert!(log[1].starts_with(&format!("{DEMO},HEAD,20")), "{}", log[1]);
    assert!(log[1].ends_with(",completed,"), "{}", log[1]);

    // Only the creator and site admins.
    assert_eq!(env.download(&env.bob, &format!("{n}.jsonl")).await.0, 403);
    assert_eq!(env.download(&env.bob, &format!("{n}.log")).await.0, 403);
    assert_eq!(env.download(&env.admin, &format!("{n}.csv")).await.0, 200);
    assert_eq!(env.download("", &format!("{n}.jsonl")).await.0, 401);
    assert_eq!(env.download(&env.alice, "999999.jsonl").await.0, 404);
    let d = env
        .ok(
            &env.bob,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({ "id": id }),
        )
        .await;
    assert!(d["node"].is_null());
    let (code, _) = env
        .err(
            &env.bob,
            "mutation($id: ID!) { deleteSearchJob(id: $id) { alwaysNil } }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(code, "FORBIDDEN");

    // Revisions by ref glob: each branch is searched on its own.
    let (glob_id, glob_n) = env
        .create(&env.alice, &format!("repo:^{DEMO}$@*refs/heads/* TODO"))
        .await;
    let job = env.wait(&env.alice, &glob_id).await;
    assert_eq!(job["state"], "COMPLETED", "{job}");
    assert_eq!(job["repoStats"]["total"], 2);
    let (_, _, body) = env.download(&env.alice, &format!("{glob_n}.jsonl")).await;
    assert!(body.contains("src/dev.go"), "{body}");
    assert_eq!(body.lines().count(), 5, "{body}");

    // A site admin reads every repository; the job runs with its creator's permissions.
    let (admin_id, admin_n) = env.create(&env.admin, "TODO type:file").await;
    let job = env.wait(&env.admin, &admin_id).await;
    assert_eq!(job["repoStats"]["total"], 2, "{job}");
    let (_, _, body) = env.download(&env.admin, &format!("{admin_n}.jsonl")).await;
    assert!(body.contains(SECRET));

    // Listing: users see their own jobs, admins everyone's.
    let list = "query($s: [SearchJobState!], $q: String, $first: Int, $after: String) {
        searchJobs(states: $s, query: $q, first: $first, after: $after, orderBy: CREATED_AT) {
            totalCount nodes { id query } pageInfo { hasNextPage hasPreviousPage endCursor }
        } }";
    let d = env.ok(&env.alice, list, json!({})).await;
    assert_eq!(d["searchJobs"]["totalCount"], 2);
    let d = env.ok(&env.bob, list, json!({})).await;
    assert_eq!(d["searchJobs"]["totalCount"], 0);
    let d = env.ok(&env.admin, list, json!({ "first": 2 })).await;
    assert_eq!(d["searchJobs"]["totalCount"], 3);
    assert_eq!(d["searchJobs"]["nodes"][0]["id"], id);
    assert_eq!(d["searchJobs"]["pageInfo"]["hasNextPage"], true);
    let after = d["searchJobs"]["pageInfo"]["endCursor"].clone();
    let d = env
        .ok(&env.admin, list, json!({ "first": 2, "after": after }))
        .await;
    assert_eq!(d["searchJobs"]["nodes"][0]["id"], admin_id);
    assert_eq!(d["searchJobs"]["pageInfo"]["hasPreviousPage"], true);
    let d = env
        .ok(
            &env.admin,
            list,
            json!({ "q": "refs/heads", "s": ["COMPLETED"] }),
        )
        .await;
    assert_eq!(d["searchJobs"]["totalCount"], 1);
    let d = env.ok(&env.admin, list, json!({ "s": ["CANCELED"] })).await;
    assert_eq!(d["searchJobs"]["totalCount"], 0);

    // Delete removes the results and the job.
    let store = SearchJobStore::new(env.db.pool().clone());
    assert!(
        !env.blobs
            .list(&format!("search-jobs/{n}/"))
            .await
            .unwrap()
            .is_empty()
    );
    env.ok(
        &env.alice,
        "mutation($id: ID!) { deleteSearchJob(id: $id) { alwaysNil } }",
        json!({ "id": id }),
    )
    .await;
    assert!(
        env.blobs
            .list(&format!("search-jobs/{n}/"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.get(n).await.unwrap().is_none());
    assert_eq!(env.download(&env.alice, &format!("{n}.jsonl")).await.0, 404);

    // The janitor removes orphaned blobs and, with a retention, finished jobs.
    sourcegull_blobstore::put_bytes(&*env.blobs, "search-jobs/424242/1.jsonl", "{}\n".into())
        .await
        .unwrap();
    let r = janitor::clean(&store, &env.blobs, None).await.unwrap();
    assert_eq!(r.orphan_blobs, 1);
    assert_eq!(r.jobs, 0);
    let r = janitor::clean(&store, &env.blobs, Some(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(r.jobs, 2);
    assert!(env.blobs.list("search-jobs/").await.unwrap().is_empty());
}

#[tokio::test]
async fn cancel_stops_queued_work() {
    // No workers: the job stays queued until canceled.
    let Some(env) = setup(false).await else {
        return;
    };
    let store = SearchJobStore::new(env.db.pool().clone());
    let alice: i64 =
        sourcegull_db::sqlx::query_scalar("SELECT id FROM users WHERE username = 'alice'")
            .fetch_one(env.db.pool())
            .await
            .unwrap();
    let job = store
        .create(sourcegull_types::UserId(alice), "TODO lang:go")
        .await
        .unwrap();
    store.cancel(job.id).await.unwrap();
    let id = sourcegull_services::relay::marshal_id("SearchJob", job.id);
    let node = env.wait(&env.alice, &id).await;
    assert_eq!(node["state"], "CANCELED");
    // A canceled job adds no more work.
    assert_eq!(
        store
            .add_repo_jobs(job.id, &[(1, "HEAD".into())])
            .await
            .unwrap(),
        0
    );
    env.ok(
        &env.alice,
        "mutation($id: ID!) { cancelSearchJob(id: $id) { alwaysNil } }",
        json!({ "id": id }),
    )
    .await;
}
