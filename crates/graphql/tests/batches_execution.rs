//! Server-side execution of bulk change specs over GraphQL against the test
//! Postgres: `executeBatchSpec` (jobs per workspace, fully cached workspaces built from the
//! cache without a job), the spec and workspace states, `cancelBatchSpecExecution`, the retry
//! mutations, permissions, a workspace's `stages` and `steps` from an executor's log and
//! the execution cache, and the apply of specs executed with `autoApplyEnabled`.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{Actor, AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpec, BatchSpecStore, BatchSpecWorkspace, BatchSpecWorkspaceStore,
    ChangesetStore, EXECUTION_JOB_VERSION, ExecutionCacheStore, ExecutionJob, ListWorkspaces,
    NewBatchSpec, NewWorkspace,
};
use sourcegull_batches_steps::{AfterStepResult, CachedStep, ChangedFiles};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, ExecutorInfo, Namespace, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_pgqueue::{ExecutionLogEntry, Queue};
use sourcegull_services::AppState;
use sourcegull_services::batches::{execution_finished, step_cache_keys};
use sourcegull_services::relay::{marshal_id, marshal_string_id};
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoId, UserId};

const ONE: &str = "gitlab.example.com/acme/one";
const TWO: &str = "gitlab.example.com/acme/two";
const THREE: &str = "gitlab.example.com/acme/three";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

const SPEC: &str = "version: 2
name: hello
on:
  - repositoriesMatchingQuery: file:README.md
steps:
  - run: echo Hello >> README.md
    container: alpine:3
    env:
      - GITLAB_TOKEN
      - PLAIN: x
    outputs:
      greeting:
        value: hello
  - run: echo done
    container: alpine:3
    if: ${{ eq repository.name \"x\" }}
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
";

const DIFF: &str = "diff --git README.md README.md\n--- README.md\n+++ README.md\n@@ -1 +1,2 @@\n # Demo\n+Hello\n";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    /// The server state, with the auto-applier.
    state: AppState,
    admin: UserId,
    alice: UserId,
    bob: UserId,
    repos: Vec<RepoId>,
}

async fn insert_repo(db: &Db, name: &str, ext: &str) -> RepoId {
    RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, private, external_id, external_service_type,
                 external_service_id, url)
             VALUES ($1, false, $2, 'gitlab', 'https://gitlab.example.com/', $3) RETURNING id",
        )
        .bind(name)
        .bind(ext)
        .bind(format!("https://{name}"))
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, SiteConfig::default());
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
    let user = |name: &'static str| {
        let db = db.clone();
        async move {
            db.users()
                .create(&NewUser {
                    username: name.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .id
        }
    };
    let alice = user("alice").await;
    let bob = user("bob").await;
    let mut repos = Vec::new();
    for (i, name) in [ONE, TWO, THREE].iter().enumerate() {
        repos.push(insert_repo(&db, name, &i.to_string()).await);
    }
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = sourcegull::features::install_auto_applier(AppState::new(
        db.clone(),
        config,
        sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle()),
        auth.clone(),
    ));
    let schema = build_schema(Services::from_state(&state));
    Some(Env {
        _t: t,
        db,
        auth,
        schema,
        state,
        admin,
        alice,
        bob,
        repos,
    })
}

fn result(step: usize) -> AfterStepResult {
    AfterStepResult {
        version: sourcegull_batches_steps::result::RESULT_VERSION,
        step_index: step,
        diff: DIFF.as_bytes().to_vec(),
        changed_files: ChangedFiles {
            modified: vec!["README.md".into()],
            ..Default::default()
        },
        stdout: "hello\n".into(),
        stderr: String::new(),
        outputs: [("greeting".to_owned(), "hello".into())].into(),
    }
}

fn cached(step: usize) -> String {
    CachedStep {
        fingerprint: "f".repeat(64),
        result: result(step),
    }
    .to_json()
}

const WORKSPACES: &str = "query($id: ID!) {
    node(id: $id) {
        ... on BatchSpec {
            state startedAt finishedAt failureMessage autoApplyEnabled viewerCanRetry
            workspaceResolution {
                workspaces(first: 10) {
                    nodes {
                        __typename
                        ... on VisibleBatchSpecWorkspace {
                            id state cachedResultFound stepCacheResultCount placeInQueue
                            placeInGlobalQueue failureMessage
                            repository { name }
                            changesetSpecs { id }
                            stages { setup { key } srcExec { key out exitCode } teardown { key } }
                            executor { hostname }
                            steps {
                                number run container ifCondition cachedResultFound skipped
                                outputLines { totalCount nodes pageInfo { hasNextPage } }
                                startedAt finishedAt exitCode
                                environment { name value }
                                outputVariables { name value }
                                diffStat { added deleted }
                            }
                            step(index: 2) { number }
                        }
                    }
                }
            }
        }
    }
}";

const EXECUTE: &str = "mutation($id: ID!, $noCache: Boolean, $auto: Boolean) {
    executeBatchSpec(batchSpec: $id, noCache: $noCache, autoApplyEnabled: $auto) {
        id state autoApplyEnabled
    }
}";

const CANCEL: &str = "mutation($id: ID!) { cancelBatchSpecExecution(batchSpec: $id) { state } }";
const RETRY: &str = "mutation($id: ID!, $all: Boolean) {
    retryBatchSpecExecution(batchSpec: $id, includeCompleted: $all) { state }
}";
const RETRY_WS: &str = "mutation($ids: [ID!]!) {
    retryBatchSpecWorkspaceExecution(batchSpecWorkspaces: $ids) { alwaysNil }
}";
const ENQUEUE_WS: &str = "mutation($ids: [ID!]!) {
    enqueueBatchSpecWorkspaceExecution(batchSpecWorkspaces: $ids) { alwaysNil }
}";

impl Env {
    async fn actor(&self, user: UserId) -> Actor {
        self.auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap()
    }

    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
        let actor = self.actor(user).await;
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, user: UserId, query: &str, vars: Value) -> (String, String) {
        let resp = self.run(user, query, vars).await;
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

    /// A server-side spec of `user`, resolved (unless `resolved` is false) into `workspaces`.
    async fn spec(&self, user: UserId, workspaces: &[NewWorkspace], resolved: bool) -> BatchSpec {
        let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
        let spec = BatchSpecStore::new(&self.db)
            .create(&NewBatchSpec {
                raw_spec: SPEC.into(),
                spec: serde_json::to_value(&parsed).unwrap(),
                namespace: Namespace::User(user),
                user_id: Some(user),
                created_from_raw: true,
                allow_unsupported: false,
                allow_ignored: false,
                no_cache: false,
                batch_change_id: None,
            })
            .await
            .unwrap();
        BatchSpecStore::new(&self.db)
            .enqueue_resolution(spec.id, user)
            .await
            .unwrap();
        if resolved {
            sourcegull_db::sqlx::query(
                "UPDATE batch_spec_resolution_jobs SET state = 'completed' WHERE batch_spec_id = $1",
            )
            .bind(spec.id)
            .execute(self.db.pool())
            .await
            .unwrap();
        }
        BatchSpecWorkspaceStore::new(&self.db)
            .create_many(spec.id, workspaces)
            .await
            .unwrap();
        spec
    }

    async fn workspaces(&self, spec: &BatchSpec) -> Vec<BatchSpecWorkspace> {
        BatchSpecWorkspaceStore::new(&self.db)
            .list(&ListWorkspaces {
                batch_spec_id: spec.id,
                ..Default::default()
            })
            .await
            .unwrap()
    }

    /// Stores both steps' results of `repo`'s root workspace in `user`'s cache and returns
    /// the workspace marked as the resolver marks a full cache hit.
    async fn cached_workspace(&self, user: UserId, repo: RepoId, name: &str) -> NewWorkspace {
        let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
        let ws = workspace(repo);
        let probe = BatchSpecWorkspace {
            id: 0,
            batch_spec_id: 0,
            repo_id: repo,
            branch: ws.branch.clone(),
            commit: ws.commit.clone(),
            path: ws.path.clone(),
            file_matches: ws.file_matches.clone(),
            only_fetch_workspace: false,
            ignored: false,
            unsupported: false,
            skipped: false,
            skipped_reason: None,
            cached_result_found: false,
            step_cache_results: json!({}),
            changeset_spec_ids: Vec::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let keys = step_cache_keys(&parsed, name, &probe);
        let mut results = serde_json::Map::new();
        for (i, key) in keys.iter().enumerate() {
            ExecutionCacheStore::new(&self.db)
                .set(user, key, &cached(i), 2)
                .await
                .unwrap();
            results.insert(
                (i + 1).to_string(),
                json!({"key": key, "value": serde_json::from_str::<Value>(&cached(i)).unwrap()}),
            );
        }
        NewWorkspace {
            cached_result_found: true,
            step_cache_results: Value::Object(results),
            ..ws
        }
    }
}

fn workspace(repo: RepoId) -> NewWorkspace {
    NewWorkspace::new(repo, "refs/heads/main", COMMIT)
}

fn spec_id(spec: &BatchSpec) -> String {
    marshal_string_id("BatchSpec", &spec.rand_id)
}

fn ws_id(ws: &BatchSpecWorkspace) -> String {
    marshal_id("BatchSpecWorkspace", ws.id)
}

fn queue(db: &Db) -> Queue<ExecutionJob> {
    Queue::new(
        db.pool().clone(),
        BatchSpecWorkspaceStore::execution_queue_config(EXECUTION_JOB_VERSION),
    )
    .unwrap()
}

/// The visible workspace nodes of a spec, as `user` sees them, by repository name.
async fn nodes(env: &Env, user: UserId, spec: &BatchSpec) -> (Value, Vec<Value>) {
    let data = env
        .ok(user, WORKSPACES, json!({ "id": spec_id(spec) }))
        .await;
    let nodes = data["node"]["workspaceResolution"]["workspaces"]["nodes"]
        .as_array()
        .unwrap()
        .clone();
    (data["node"].clone(), nodes)
}

fn by_repo<'a>(nodes: &'a [Value], name: &str) -> &'a Value {
    nodes
        .iter()
        .find(|n| n["repository"]["name"] == name)
        .unwrap_or_else(|| panic!("no workspace in {name}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_with_cached_workspaces_and_permissions() {
    let Some(env) = setup().await else {
        return;
    };
    let cached = env.cached_workspace(env.alice, env.repos[0], ONE).await;
    let mut skipped = workspace(env.repos[2]);
    skipped.skipped = true;
    skipped.skipped_reason = Some("The repository is archived.".into());
    let spec = env
        .spec(env.alice, &[cached, workspace(env.repos[1]), skipped], true)
        .await;
    let id = spec_id(&spec);
    let (node, _) = nodes(&env, env.alice, &spec).await;
    assert_eq!(node["state"], "PENDING");

    // Only the creator (or a site admin) executes.
    let (code, _) = env.err(env.bob, EXECUTE, json!({ "id": id })).await;
    assert_eq!(code, "FORBIDDEN");
    let data = env
        .ok(env.alice, EXECUTE, json!({ "id": id, "auto": true }))
        .await;
    assert_eq!(data["executeBatchSpec"]["state"], "QUEUED");
    assert_eq!(data["executeBatchSpec"]["autoApplyEnabled"], true);
    let (_, message) = env.err(env.alice, EXECUTE, json!({ "id": id })).await;
    assert!(message.contains("already been executed"), "{message}");

    let (node, ws) = nodes(&env, env.alice, &spec).await;
    assert_eq!(node["failureMessage"], Value::Null);
    assert_eq!(node["viewerCanRetry"], false);
    // The cached workspace has its changeset spec and no execution.
    let one = by_repo(&ws, ONE);
    assert_eq!(one["state"], "COMPLETED");
    assert_eq!(one["cachedResultFound"], true);
    assert_eq!(one["stepCacheResultCount"], 2);
    assert_eq!(one["changesetSpecs"].as_array().unwrap().len(), 1);
    assert_eq!(one["stages"], Value::Null);
    assert_eq!(one["steps"][0]["cachedResultFound"], true);
    assert_eq!(one["steps"][0]["diffStat"]["added"], 1);
    // The other one is queued.
    let two = by_repo(&ws, TWO);
    assert_eq!(two["state"], "QUEUED");
    assert_eq!(two["placeInQueue"], 1);
    assert_eq!(two["placeInGlobalQueue"], 1);
    assert_eq!(two["changesetSpecs"].as_array().unwrap().len(), 0);
    assert_eq!(by_repo(&ws, THREE)["state"], "SKIPPED");
    let stats = BatchSpecWorkspaceStore::new(&env.db)
        .stats(spec.id)
        .await
        .unwrap();
    assert_eq!(stats.executions, 1);

    // Specs that are not resolved, or not server-side, cannot be executed.
    let pending = env.spec(env.alice, &[workspace(env.repos[1])], false).await;
    let (_, message) = env
        .err(env.alice, EXECUTE, json!({ "id": spec_id(&pending) }))
        .await;
    assert!(message.contains("still being resolved"), "{message}");
    let local = BatchSpecStore::new(&env.db)
        .create(&NewBatchSpec {
            raw_spec: SPEC.into(),
            spec: json!({}),
            namespace: Namespace::User(env.alice),
            user_id: Some(env.alice),
            created_from_raw: false,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap();
    let (code, _) = env
        .err(env.alice, EXECUTE, json!({ "id": spec_id(&local) }))
        .await;
    assert_eq!(code, "INVALID_INPUT");

    // A site admin may execute someone else's spec (it runs as its creator); with noCache
    // the cached workspace runs too.
    let other = env
        .spec(
            env.alice,
            &[env.cached_workspace(env.alice, env.repos[0], ONE).await],
            true,
        )
        .await;
    let data = env
        .ok(
            env.admin,
            EXECUTE,
            json!({ "id": spec_id(&other), "noCache": true }),
        )
        .await;
    assert_eq!(data["executeBatchSpec"]["state"], "QUEUED");
    let ws = env.workspaces(&other).await;
    assert!(!ws[0].cached_result_found);
    let job = BatchSpecWorkspaceStore::new(&env.db)
        .execution_jobs_for_workspaces(&[ws[0].id])
        .await
        .unwrap();
    assert_eq!(job[0].payload.user_id, env.alice);

    // Everything cached: completed at once.
    let all_cached = env
        .spec(
            env.alice,
            &[env.cached_workspace(env.alice, env.repos[0], ONE).await],
            true,
        )
        .await;
    let data = env
        .ok(env.alice, EXECUTE, json!({ "id": spec_id(&all_cached) }))
        .await;
    assert_eq!(data["executeBatchSpec"]["state"], "COMPLETED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stages_steps_failure_and_retry() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(env.alice, &[workspace(env.repos[0])], true).await;
    let id = spec_id(&spec);
    env.ok(env.alice, EXECUTE, json!({ "id": id })).await;

    // An executor takes the job and reports progress.
    let q = queue(&env.db);
    let job = q.dequeue("exec-1", 1).await.unwrap().pop().unwrap();
    env.db
        .executors()
        .heartbeat(
            &ExecutorInfo {
                hostname: "exec-1".into(),
                queue_names: vec!["batches".into()],
                os: "linux".into(),
                architecture: "amd64".into(),
                executor_version: "0.1.0".into(),
                runtime: "docker".into(),
                runtime_version: "27".into(),
                git_version: "2.45".into(),
            },
            &[],
        )
        .await
        .unwrap();
    let started = chrono::Utc::now();
    for entry in [
        ExecutionLogEntry {
            key: "setup.git.fetch".into(),
            command: vec!["git".into(), "fetch".into()],
            exit_code: Some(0),
            out: "fetched".into(),
            ..Default::default()
        },
        ExecutionLogEntry {
            key: "step.0".into(),
            command: vec!["alpine:3".into(), "echo Hello >> README.md".into()],
            start_time: Some(started),
            exit_code: Some(0),
            out: "stdout: hello\nstderr: warning\n".into(),
            duration_ms: Some(1500),
        },
        ExecutionLogEntry {
            key: "step.1.skipped".into(),
            ..Default::default()
        },
        ExecutionLogEntry {
            key: "teardown.fs".into(),
            ..Default::default()
        },
    ] {
        q.add_execution_log(job.id, &entry).await.unwrap();
    }
    let ws = env.workspaces(&spec).await.remove(0);
    let keys = step_cache_keys(
        &sourcegull_batches::BatchSpec::parse(SPEC).unwrap(),
        ONE,
        &ws,
    );
    ExecutionCacheStore::new(&env.db)
        .set(env.alice, &keys[0], &cached(0), 2)
        .await
        .unwrap();

    let (node, ws_nodes) = nodes(&env, env.alice, &spec).await;
    assert_eq!(node["state"], "PROCESSING");
    assert!(node["startedAt"].is_string());
    assert_eq!(node["finishedAt"], Value::Null);
    let w = &ws_nodes[0];
    assert_eq!(w["state"], "PROCESSING");
    assert_eq!(w["executor"], Value::Null, "site admins only");
    assert_eq!(w["stages"]["setup"][0]["key"], "setup.git.fetch");
    assert_eq!(
        w["stages"]["srcExec"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["step.0", "step.1.skipped"]
    );
    assert_eq!(
        w["stages"]["srcExec"][0]["out"],
        "stdout: hello\nstderr: warning\n"
    );
    assert_eq!(w["stages"]["teardown"][0]["key"], "teardown.fs");
    let s0 = &w["steps"][0];
    assert_eq!(s0["number"], 1);
    assert_eq!(s0["run"], "echo Hello >> README.md");
    assert_eq!(s0["container"], "alpine:3");
    assert_eq!(s0["ifCondition"], Value::Null);
    assert_eq!(s0["cachedResultFound"], false);
    assert_eq!(s0["skipped"], false);
    assert_eq!(s0["exitCode"], 0);
    assert!(s0["finishedAt"].is_string());
    assert_eq!(s0["outputLines"]["totalCount"], 2);
    assert_eq!(
        s0["outputLines"]["nodes"],
        json!(["stdout: hello", "stderr: warning"])
    );
    assert_eq!(
        s0["environment"],
        json!([{"name": "GITLAB_TOKEN", "value": null}, {"name": "PLAIN", "value": "x"}])
    );
    assert_eq!(
        s0["outputVariables"],
        json!([{"name": "greeting", "value": "hello"}])
    );
    assert_eq!(s0["diffStat"], json!({"added": 1, "deleted": 0}));
    let s1 = &w["steps"][1];
    assert_eq!(s1["skipped"], true);
    assert_eq!(s1["ifCondition"], "${{ eq repository.name \"x\" }}");
    assert_eq!(s1["outputVariables"], Value::Null);
    assert_eq!(s1["startedAt"], Value::Null);
    assert_eq!(w["step"]["number"], 2);

    // Others see the steps but not the output; site admins see the executor.
    let (_, ws_nodes) = nodes(&env, env.bob, &spec).await;
    assert_eq!(ws_nodes[0]["stages"]["srcExec"][0]["out"], "");
    assert_eq!(ws_nodes[0]["steps"][0]["outputLines"]["totalCount"], 0);
    let (_, ws_nodes) = nodes(&env, env.admin, &spec).await;
    assert_eq!(ws_nodes[0]["executor"]["hostname"], "exec-1");
    assert_eq!(
        ws_nodes[0]["stages"]["srcExec"][0]["out"],
        "stdout: hello\nstderr: warning\n"
    );

    // Retrying needs a finished execution.
    let (_, message) = env.err(env.alice, RETRY, json!({ "id": id })).await;
    assert!(message.contains("not finished"), "{message}");

    q.mark_failed(job.id, "exec-1", "step 1 exited with 1")
        .await
        .unwrap();
    let (node, ws_nodes) = nodes(&env, env.alice, &spec).await;
    assert_eq!(node["state"], "FAILED");
    assert!(node["finishedAt"].is_string());
    assert_eq!(
        node["failureMessage"],
        "Failures:\n\n* step 1 exited with 1\n"
    );
    assert_eq!(node["viewerCanRetry"], true);
    assert_eq!(ws_nodes[0]["failureMessage"], "step 1 exited with 1");
    let (node, _) = nodes(&env, env.bob, &spec).await;
    assert_eq!(node["viewerCanRetry"], false);
    let (code, _) = env.err(env.bob, RETRY, json!({ "id": id })).await;
    assert_eq!(code, "FORBIDDEN");

    let data = env.ok(env.alice, RETRY, json!({ "id": id })).await;
    assert_eq!(data["retryBatchSpecExecution"]["state"], "QUEUED");
    let (_, ws_nodes) = nodes(&env, env.alice, &spec).await;
    assert_eq!(ws_nodes[0]["stages"]["srcExec"], json!([]));
    assert_eq!(ws_nodes[0]["failureMessage"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_workspace_retries() {
    let Some(env) = setup().await else {
        return;
    };
    let cached = env.cached_workspace(env.alice, env.repos[0], ONE).await;
    let spec = env
        .spec(
            env.alice,
            &[cached, workspace(env.repos[1]), workspace(env.repos[2])],
            true,
        )
        .await;
    let id = spec_id(&spec);
    let (_, message) = env.err(env.alice, CANCEL, json!({ "id": id })).await;
    assert!(message.contains("cannot be canceled"), "{message}");
    env.ok(env.alice, EXECUTE, json!({ "id": id })).await;

    // One job runs; canceling cancels the queued one and flags the running one.
    let q = queue(&env.db);
    let running = q.dequeue("exec-1", 1).await.unwrap().pop().unwrap();
    let (code, _) = env.err(env.bob, CANCEL, json!({ "id": id })).await;
    assert_eq!(code, "FORBIDDEN");
    let data = env.ok(env.alice, CANCEL, json!({ "id": id })).await;
    assert_eq!(data["cancelBatchSpecExecution"]["state"], "CANCELING");
    let hb = q.heartbeat(&[running.id], "exec-1").await.unwrap();
    assert_eq!(hb.canceled, [running.id]);
    q.mark_canceled(running.id, "exec-1").await.unwrap();
    let (node, ws) = nodes(&env, env.alice, &spec).await;
    assert_eq!(node["state"], "CANCELED");
    assert_eq!(node["viewerCanRetry"], true);
    assert_eq!(by_repo(&ws, TWO)["state"], "CANCELED");
    let (_, message) = env.err(env.alice, CANCEL, json!({ "id": id })).await;
    assert!(message.contains("cannot be canceled"), "{message}");

    // Retry one workspace, then it is queued and cannot be retried again.
    let rows = env.workspaces(&spec).await;
    let two = rows.iter().find(|w| w.repo_id == env.repos[1]).unwrap();
    let one = rows.iter().find(|w| w.repo_id == env.repos[0]).unwrap();
    env.ok(env.alice, RETRY_WS, json!({ "ids": [ws_id(two)] }))
        .await;
    let (_, ws) = nodes(&env, env.alice, &spec).await;
    assert_eq!(by_repo(&ws, TWO)["state"], "QUEUED");
    assert_eq!(by_repo(&ws, THREE)["state"], "CANCELED");
    let (_, message) = env
        .err(env.alice, RETRY_WS, json!({ "ids": [ws_id(two)] }))
        .await;
    assert!(message.contains("not finished"), "{message}");
    // The cached workspace has no execution to retry, but can be enqueued to run anyway.
    let (_, message) = env
        .err(env.alice, RETRY_WS, json!({ "ids": [ws_id(one)] }))
        .await;
    assert!(message.contains("not been executed"), "{message}");
    let (code, _) = env
        .err(env.bob, ENQUEUE_WS, json!({ "ids": [ws_id(one)] }))
        .await;
    assert_eq!(code, "FORBIDDEN");
    env.ok(env.alice, ENQUEUE_WS, json!({ "ids": [ws_id(one)] }))
        .await;
    let (node, ws) = nodes(&env, env.alice, &spec).await;
    assert_eq!(by_repo(&ws, ONE)["state"], "QUEUED");
    assert_eq!(node["state"], "PROCESSING");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_apply_once_the_execution_completed() {
    let Some(env) = setup().await else {
        return;
    };
    let applied = |env: &Env| {
        let db = env.db.clone();
        let alice = env.alice;
        async move {
            BatchChangeStore::new(&db)
                .get_by_name(Namespace::User(alice), "hello")
                .await
                .unwrap()
        }
    };

    // Executed without autoApplyEnabled: nothing is applied.
    let manual = env
        .spec(
            env.alice,
            &[env.cached_workspace(env.alice, env.repos[0], ONE).await],
            true,
        )
        .await;
    env.ok(env.alice, EXECUTE, json!({ "id": spec_id(&manual) }))
        .await;
    assert!(applied(&env).await.is_none());

    // Everything cached: the execution completes at once and the spec is applied as its
    // creator, even when a site admin executed it.
    let cached = env
        .spec(
            env.alice,
            &[env.cached_workspace(env.alice, env.repos[0], ONE).await],
            true,
        )
        .await;
    let data = env
        .ok(
            env.admin,
            EXECUTE,
            json!({ "id": spec_id(&cached), "auto": true }),
        )
        .await;
    assert_eq!(data["executeBatchSpec"]["state"], "COMPLETED");
    let bc = applied(&env).await.expect("applied");
    assert_eq!(bc.batch_spec_id, cached.id);
    assert_eq!(bc.last_applier_id, Some(env.alice));
    let changesets = ChangesetStore::new(&env.db)
        .list(
            &sourcegull_batches::store::ListChangesets {
                batch_change_id: Some(bc.id),
                ..Default::default()
            },
            &sourcegull_types::RepoAccess::All,
        )
        .await
        .unwrap();
    assert_eq!(changesets.len(), 1);
    let spec = BatchSpecStore::new(&env.db)
        .get(cached.id)
        .await
        .unwrap()
        .unwrap();
    assert!(spec.auto_applied_at.is_some());

    // With an execution job: applied when the job completed, once.
    let run = env
        .spec(
            env.alice,
            &[env.cached_workspace(env.alice, env.repos[0], ONE).await],
            true,
        )
        .await;
    env.ok(
        env.alice,
        EXECUTE,
        json!({ "id": spec_id(&run), "noCache": true, "auto": true }),
    )
    .await;
    let finished = |env: &Env, id: i64| {
        let db = env.db.clone();
        let applier = env.state.auto_applier.clone();
        async move { execution_finished(&db, applier.as_ref(), id).await.unwrap() }
    };
    assert!(!finished(&env, run.id).await);
    assert_eq!(applied(&env).await.unwrap().batch_spec_id, cached.id);
    let q = queue(&env.db);
    let job = q.dequeue("exec-1", 1).await.unwrap().pop().unwrap();
    assert!(q.mark_complete(job.id, "exec-1").await.unwrap());
    assert!(finished(&env, run.id).await);
    let bc = applied(&env).await.unwrap();
    assert_eq!(bc.batch_spec_id, run.id);
    let first_apply = bc.last_applied_at;
    assert!(finished(&env, run.id).await);
    assert_eq!(applied(&env).await.unwrap().last_applied_at, first_apply);
}
