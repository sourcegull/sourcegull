//! Auto-indexing over GraphQL against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`)
//! and an in-process repo node: inference and the inference script, a repository's index
//! configuration, queueing jobs, and index jobs in `preciseIndexes` (states, steps and logs,
//! the upload a job made, reindex and delete).

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_codeintel_scip::store::{CodeIntelStore, IndexJobStore, NewUpload};
use sourcegull_config::{ConfigHandle, SiteConfig, SiteConfigHandle};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const NAME: &str = "gitlab.example.com/acme/app";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    db: Db,
    auth: Authenticator,
    config: SiteConfigHandle,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    repo: RepoId,
    head: String,
}

fn enabled(on: bool) -> SiteConfig {
    SiteConfig {
        code_intel_auto_indexing_enabled: on,
        ..SiteConfig::default()
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, enabled(true));
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let admin = auth
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let src = SourceRepo::new();
    let head = src.commit(
        "init",
        &[
            ("go.mod", "module app\n"),
            ("main.go", "package main\n"),
            ("web/package.json", "{}"),
            ("web/tsconfig.json", "{}"),
        ],
    );
    let node = RepoNode::start(src.remote()).await;
    let repo = RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
             VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(NAME)
        .fetch_one(db.pool())
        .await
        .unwrap(),
    );
    node.clone_repo(repo, NAME).await;
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        db,
        auth,
        config,
        schema,
        admin,
        alice,
        repo,
        head,
    })
}

impl Env {
    async fn run(&self, actor: UserId, query: &str, vars: Value) -> Response {
        let actor = self.auth.actor_for(actor, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, actor: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, actor: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(actor, query, vars).await;
        let e = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        let code = e
            .extensions
            .as_ref()
            .and_then(|x| x.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default();
        (code, e.message.clone())
    }

    fn repo_id(&self) -> String {
        marshal_id("Repository", i64::from(self.repo.get()))
    }

    async fn list(&self, actor: UserId, vars: Value) -> Value {
        let mut vars = vars;
        vars["repo"] = json!(self.repo_id());
        self.ok(actor, LIST, vars).await["preciseIndexes"].clone()
    }
}

const INFER: &str = "query($repo: ID!, $script: String) {
    inferAutoIndexJobsForRepo(repository: $repo, script: $script) {
        jobs { root comparisonKey indexer { key name imageName }
               steps { preIndex { root image commands } index { indexerArgs outfile requestedEnvVars } } }
        inferenceOutput
    }
}";

const LIST: &str = "query($repo: ID, $states: [PreciseIndexState!], $first: Int, $after: String) {
    preciseIndexes(repo: $repo, states: $states, first: $first, after: $after) {
        totalCount pageInfo { hasNextPage endCursor }
        nodes { id state inputRoot inputIndexer shouldReindex failure queuedAt uploadedAt placeInQueue
            indexer { key name }
            steps { setup { key out } preIndex { image logEntry { key exitCode out } }
                    index { indexerArgs logEntry { key exitCode out durationMilliseconds } }
                    upload { key } } }
    }
}";

#[tokio::test]
async fn inference_and_the_inference_script() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let vars = json!({ "repo": env.repo_id() });
    let r = env.ok(env.admin, INFER, vars.clone()).await;
    let jobs = r["inferAutoIndexJobsForRepo"]["jobs"].as_array().unwrap();
    let keys: Vec<(&str, &str)> = jobs
        .iter()
        .map(|j| {
            (
                j["indexer"]["key"].as_str().unwrap(),
                j["root"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(keys, [("go", ""), ("typescript", "web")]);
    let go = &jobs[0];
    assert_eq!(go["indexer"]["name"], "scip-go");
    assert!(
        go["indexer"]["imageName"]
            .as_str()
            .unwrap()
            .starts_with("sourcegraph/scip-go@sha256:")
    );
    assert_eq!(
        go["steps"]["index"]["indexerArgs"],
        json!(["scip-go", "--no-animation"])
    );
    assert!(
        go["steps"]["index"]["requestedEnvVars"]
            .as_array()
            .unwrap()
            .contains(&json!("NETRC_DATA"))
    );
    assert_eq!(
        jobs[1]["steps"]["preIndex"][0]["commands"],
        json!(["npm install"])
    );

    // Only site admins, and only with auto-indexing enabled.
    assert_eq!(env.err(env.alice, INFER, vars.clone()).await.0, "FORBIDDEN");
    env.config.store(2, enabled(false));
    let (code, msg) = env.err(env.admin, INFER, vars.clone()).await;
    assert_eq!(code, "FORBIDDEN");
    assert!(msg.contains("not enabled"), "{msg}");
    env.config.store(3, enabled(true));

    // A trial script, then the site's script.
    let script = r#"print("hello from lua") return require("sg.autoindex.config").new({ ["sg.typescript"] = false })"#;
    let r = env
        .ok(
            env.admin,
            INFER,
            json!({ "repo": env.repo_id(), "script": script }),
        )
        .await;
    assert_eq!(
        r["inferAutoIndexJobsForRepo"]["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        r["inferAutoIndexJobsForRepo"]["inferenceOutput"],
        "hello from lua\n"
    );

    const UPDATE: &str =
        "mutation($s: String!) { updateCodeIntelligenceInferenceScript(script: $s) { alwaysNil } }";
    let (code, msg) = env
        .err(env.admin, UPDATE, json!({ "s": "while true do end" }))
        .await;
    assert_eq!(code, "TOO_LARGE", "{msg}");
    assert_eq!(
        env.err(env.admin, UPDATE, json!({ "s": "return 1" }))
            .await
            .0,
        "INVALID_INPUT"
    );
    assert_eq!(
        env.err(env.alice, UPDATE, json!({ "s": script })).await.0,
        "FORBIDDEN"
    );
    env.ok(env.admin, UPDATE, json!({ "s": script })).await;
    let r = env
        .ok(env.admin, "{ codeIntelligenceInferenceScript }", json!({}))
        .await;
    assert_eq!(r["codeIntelligenceInferenceScript"], script);
    let r = env.ok(env.admin, INFER, vars).await;
    assert_eq!(
        r["inferAutoIndexJobsForRepo"]["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn repository_index_configuration() {
    let Some(env) = setup().await else { return };
    const GET: &str = "query($repo: ID!) { node(id: $repo) { ... on Repository {
        indexConfiguration { configuration parsedConfiguration { root indexer { name } }
            inferredConfiguration { configuration parsedConfiguration { root } limitError } } } } }";
    const SET: &str = "mutation($repo: ID!, $c: String!) {
        updateRepositoryIndexConfiguration(repository: $repo, configuration: $c) { alwaysNil } }";
    let vars = json!({ "repo": env.repo_id() });
    let r = env.ok(env.alice, GET, vars.clone()).await;
    let c = &r["node"]["indexConfiguration"];
    assert_eq!(c["configuration"], Value::Null);
    assert_eq!(c["parsedConfiguration"], Value::Null);
    let inferred = &c["inferredConfiguration"];
    assert_eq!(inferred["limitError"], Value::Null);
    assert_eq!(
        inferred["parsedConfiguration"],
        json!([{ "root": "" }, { "root": "web" }])
    );
    assert!(
        inferred["configuration"]
            .as_str()
            .unwrap()
            .contains("\"index_jobs\"")
    );

    let config = r#"{"index_jobs": [{"indexer": "sourcegraph/scip-go", "root": "cmd", "indexer_args": ["scip-go"]}]}"#;
    let set = json!({ "repo": env.repo_id(), "c": config });
    assert_eq!(env.err(env.alice, SET, set.clone()).await.0, "FORBIDDEN");
    let bad = json!({ "repo": env.repo_id(), "c": "{nope" });
    assert_eq!(env.err(env.admin, SET, bad).await.0, "INVALID_INPUT");
    env.ok(env.admin, SET, set).await;
    let r = env.ok(env.admin, GET, vars).await;
    let c = &r["node"]["indexConfiguration"];
    assert_eq!(c["configuration"], config);
    assert_eq!(
        c["parsedConfiguration"],
        json!([{ "root": "cmd", "indexer": { "name": "scip-go" } }])
    );

    // The saved configuration is what queueing uses.
    const QUEUE: &str =
        "mutation($repo: ID!) { queueAutoIndexJobsForRepo(repository: $repo) { inputRoot state } }";
    let r = env
        .ok(env.admin, QUEUE, json!({ "repo": env.repo_id() }))
        .await;
    assert_eq!(
        r["queueAutoIndexJobsForRepo"],
        json!([{ "inputRoot": "cmd/", "state": "QUEUED_FOR_INDEXING" }])
    );
}

#[tokio::test]
async fn index_jobs_in_precise_indexes() {
    let Some(env) = setup().await else { return };
    const QUEUE: &str = "mutation($repo: ID!, $c: String) {
        queueAutoIndexJobsForRepo(repository: $repo, configuration: $c) { id state inputCommit } }";
    assert_eq!(
        env.err(env.alice, QUEUE, json!({ "repo": env.repo_id() }))
            .await
            .0,
        "FORBIDDEN"
    );
    let r = env
        .ok(env.admin, QUEUE, json!({ "repo": env.repo_id() }))
        .await;
    let queued = r["queueAutoIndexJobsForRepo"].as_array().unwrap().clone();
    assert_eq!(queued.len(), 2);
    assert!(queued.iter().all(|q| q["state"] == "QUEUED_FOR_INDEXING"));
    assert_eq!(queued[0]["inputCommit"], env.head.as_str());

    // Readers see the jobs, with their steps.
    let page = env.list(env.alice, json!({})).await;
    assert_eq!(page["totalCount"], 2);
    let nodes = page["nodes"].as_array().unwrap();
    let ts = nodes.iter().find(|n| n["inputRoot"] == "web/").unwrap();
    assert_eq!(ts["indexer"]["key"], "typescript");
    assert_eq!(ts["uploadedAt"], Value::Null);
    assert!(ts["queuedAt"].is_string());
    assert!(ts["placeInQueue"].is_number());
    assert_eq!(ts["steps"]["preIndex"].as_array().unwrap().len(), 1);
    assert_eq!(ts["steps"]["upload"], Value::Null);
    let only_queued = env
        .list(env.alice, json!({ "states": ["QUEUED_FOR_INDEXING"] }))
        .await;
    assert_eq!(only_queued["totalCount"], 2);
    let none = env
        .list(env.alice, json!({ "states": ["COMPLETED"] }))
        .await;
    assert_eq!(none["totalCount"], 0);

    // An executor ran the Go job and it failed: logs, output for site admins only.
    let go_id = IndexJobStore::new(&env.db)
        .list(
            &sourcegull_codeintel_scip::store::ListIndexJobs {
                limit: 100,
                ..Default::default()
            },
            &sourcegull_types::RepoAccess::All,
        )
        .await
        .unwrap()
        .0
        .into_iter()
        .find(|j| j.job.root.is_empty())
        .unwrap()
        .id;
    sourcegull_db::sqlx::query(
        r#"UPDATE codeintel_index_jobs SET state = 'failed', failure_message = 'exit 1',
            started_at = now(), finished_at = now(), execution_logs = $2::jsonb WHERE id = $1"#,
    )
    .bind(go_id)
    .bind(json!([
        {"key": "setup.git.fetch", "command": ["git", "fetch"], "start_time": null, "exit_code": 0, "out": "fetched", "duration_ms": 5},
        {"key": "step.docker.pre-index.0", "command": ["sh"], "start_time": null, "exit_code": 0, "out": "go: downloading", "duration_ms": 9},
        {"key": "step.docker.indexer", "command": ["sh"], "start_time": null, "exit_code": 1, "out": "boom", "duration_ms": 12}
    ]))
    .execute(env.db.pool())
    .await
    .unwrap();
    let errored = env
        .list(env.admin, json!({ "states": ["INDEXING_ERRORED"] }))
        .await;
    let go = &errored["nodes"][0];
    assert_eq!(go["state"], "INDEXING_ERRORED");
    assert_eq!(go["failure"], "exit 1");
    assert_eq!(
        go["steps"]["setup"],
        json!([{ "key": "setup.git.fetch", "out": "fetched" }])
    );
    assert_eq!(
        go["steps"]["preIndex"][0]["logEntry"]["out"],
        "go: downloading"
    );
    assert_eq!(
        go["steps"]["index"]["logEntry"],
        json!({ "key": "step.docker.indexer", "exitCode": 1, "out": "boom", "durationMilliseconds": 12 })
    );
    let as_alice = env
        .list(env.alice, json!({ "states": ["INDEXING_ERRORED"] }))
        .await;
    assert_eq!(
        as_alice["nodes"][0]["steps"]["index"]["logEntry"]["out"],
        ""
    );

    // Reindexing a failed job marks it and queues it again.
    const REINDEX: &str = "mutation($id: ID!) { reindexPreciseIndex(id: $id) { alwaysNil } }";
    let go_node = go["id"].as_str().unwrap().to_owned();
    assert_eq!(
        env.err(env.alice, REINDEX, json!({ "id": go_node }))
            .await
            .0,
        "FORBIDDEN"
    );
    env.ok(env.admin, REINDEX, json!({ "id": go_node })).await;
    let page = env.list(env.admin, json!({})).await;
    assert_eq!(page["totalCount"], 3);
    let old = page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == go_node.as_str())
        .unwrap();
    assert_eq!(old["shouldReindex"], true);

    // The job uploads an index: the upload stands for both.
    let upload = CodeIntelStore::new(&env.db)
        .create_upload(&NewUpload {
            repo_id: env.repo,
            commit: env.head.clone(),
            root: "web/".into(),
            indexer: "scip-typescript".into(),
            indexer_version: "0.4".into(),
            num_parts: 1,
            uploader: None,
            associated_index_id: None,
            ci_job: None,
        })
        .await
        .unwrap();
    let ts_job = IndexJobStore::new(&env.db)
        .list(
            &sourcegull_codeintel_scip::store::ListIndexJobs {
                limit: 100,
                ..Default::default()
            },
            &sourcegull_types::RepoAccess::All,
        )
        .await
        .unwrap()
        .0
        .into_iter()
        .find(|j| j.job.root == "web/")
        .unwrap()
        .id;
    sourcegull_db::sqlx::query("UPDATE codeintel_uploads SET associated_index_id = $2, state = 'completed', upload_size = 1 WHERE id = $1")
        .bind(upload)
        .bind(ts_job)
        .execute(env.db.pool())
        .await
        .unwrap();
    sourcegull_db::sqlx::query("UPDATE codeintel_index_jobs SET state = 'completed' WHERE id = $1")
        .bind(ts_job)
        .execute(env.db.pool())
        .await
        .unwrap();
    IndexJobStore::new(&env.db)
        .set_associated_upload(ts_job, upload)
        .await
        .unwrap();
    let page = env.list(env.admin, json!({})).await;
    assert_eq!(page["totalCount"], 3);
    let merged = page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["inputRoot"] == "web/")
        .unwrap()
        .clone();
    assert_eq!(merged["state"], "COMPLETED");
    assert!(merged["steps"].is_object());
    assert!(merged["queuedAt"].is_string() && merged["uploadedAt"].is_string());
    let r = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { ... on PreciseIndex { id state inputRoot } } }",
            json!({ "id": merged["id"] }),
        )
        .await;
    assert_eq!(r["node"]["state"], "COMPLETED");

    // Reindexing it marks the upload and queues the job again.
    env.ok(env.admin, REINDEX, json!({ "id": merged["id"] }))
        .await;
    let page = env.list(env.admin, json!({})).await;
    assert_eq!(page["totalCount"], 4);
    let merged = page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == merged["id"])
        .unwrap()
        .clone();
    assert_eq!(merged["shouldReindex"], true);

    // Pages merge uploads and jobs.
    let first = env.list(env.admin, json!({ "first": 3 })).await;
    assert_eq!(first["nodes"].as_array().unwrap().len(), 3);
    assert_eq!(first["pageInfo"]["hasNextPage"], true);
    let rest = env
        .list(
            env.admin,
            json!({ "first": 3, "after": first["pageInfo"]["endCursor"] }),
        )
        .await;
    assert_eq!(rest["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(rest["pageInfo"]["hasNextPage"], false);
    let mut ids: Vec<Value> = first["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].clone())
        .collect();
    ids.push(rest["nodes"][0]["id"].clone());
    ids.sort_by_key(|v| v.to_string());
    ids.dedup();
    assert_eq!(ids.len(), 4);

    // Deleting a job without an upload removes it.
    const DELETE: &str = "mutation($id: ID!) { deletePreciseIndex(id: $id) { alwaysNil } }";
    env.ok(env.admin, DELETE, json!({ "id": go_node })).await;
    assert_eq!(env.list(env.admin, json!({})).await["totalCount"], 3);
    const DELETE_ALL: &str = "mutation($repo: ID) { deletePreciseIndexes(repository: $repo, states: [QUEUED_FOR_INDEXING]) { alwaysNil } }";
    env.ok(env.admin, DELETE_ALL, json!({ "repo": env.repo_id() }))
        .await;
    let page = env.list(env.admin, json!({})).await;
    assert_eq!(page["totalCount"], 1);
    assert_eq!(page["nodes"][0]["state"], "COMPLETED");
}
