//! The `batches` executor queue against the test Postgres, with a
//! fake executor speaking the HTTP protocol: the dequeued payload, executor secret resolution
//! and access logs, step cache uploads and their reuse by a later execution, changeset spec
//! uploads, cancellation through the heartbeat, and fair dequeueing between users.

// Tests insert repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_batches::store::{
    BatchSpec, BatchSpecStore, BatchSpecWorkspaceStore, ChangesetSpecStore, ExecutionCacheStore,
    ListWorkspaces, NewBatchSpec, NewWorkspace,
};
use sourcegull_batches_steps::{
    AfterStepResult, CachedStep, ChangedFiles, ChangesetSpecInput, SpecRepository,
    build_changeset_specs,
};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ExecutorSecretScope, Namespace, NewUser, migrate};
use sourcegull_executor_api::{
    BatchesResultResponse, DequeueRequest, HeartbeatRequest, HeartbeatResponse, Job, QueueJobIds,
    capability,
};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_pgqueue::JobState;
use sourcegull_services::AppState;
use sourcegull_services::batches::{ExecuteOptions, execute_batch_spec, step_cache_keys};
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;

const REPO: &str = "gitlab.example.com/acme/demo";
const OTHER: &str = "gitlab.example.com/acme/other";
const TOKEN: &str = "executor-secret-0123456789";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BATCHES: &[&str] = &[capability::BATCH_STEPS, capability::SECRETS];

const SPEC: &str = "version: 2
name: hello
description: Say hello
on:
  - repository: gitlab.example.com/acme/demo
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
changesetTemplate:
  title: Hello
  body: Say hello
  branch: hello
  commit:
    message: Say hello
  published: false
";

const DIFF: &str = "diff --git README.md README.md\n--- README.md\n+++ README.md\n@@ -1 +1,2 @@\n # Demo\n+Hello\n";

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    base: String,
    http: reqwest::Client,
    alice: UserId,
    bob: UserId,
    repo: RepoId,
    other: RepoId,
    _cancel: tokio_util::sync::DropGuard,
}

async fn insert_repo(db: &Db, name: &str, ext: &str) -> RepoId {
    RepoId(
        sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
             VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(name)
        .bind(ext)
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let repo = insert_repo(&db, REPO, "1").await;
    let other = insert_repo(&db, OTHER, "2").await;
    let alice = user(&db, "alice").await;
    let bob = user(&db, "bob").await;
    db.user_emails()
        .add(alice, "alice@example.com")
        .await
        .unwrap();
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            executors_access_token: Some(TOKEN.into()),
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let provider = Arc::new(StaticKeyProvider::new("k1", [7u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_keyring(Some(keyring.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cancel = CancellationToken::new();
    tokio::spawn(sourcegull_http_api::serve(
        listener,
        public_router(state),
        cancel.clone(),
    ));
    Some(Env {
        _t: t,
        db,
        keyring,
        base,
        http: reqwest::Client::new(),
        alice,
        bob,
        repo,
        other,
        _cancel: cancel.drop_guard(),
    })
}

impl Env {
    /// A resolved server-side spec of `user` with one root workspace per repository.
    async fn spec(&self, user: UserId, raw: &str, repos: &[RepoId]) -> BatchSpec {
        self.spec_with(
            user,
            raw,
            repos.iter().map(|r| self.workspace(*r)).collect(),
        )
        .await
    }

    fn workspace(&self, repo: RepoId) -> NewWorkspace {
        NewWorkspace::new(repo, "refs/heads/main", COMMIT)
    }

    async fn spec_with(&self, user: UserId, raw: &str, workspaces: Vec<NewWorkspace>) -> BatchSpec {
        let parsed = sourcegull_batches::BatchSpec::parse(raw).unwrap();
        let spec = BatchSpecStore::new(&self.db)
            .create(&NewBatchSpec {
                raw_spec: raw.into(),
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
        sqlx::query(
            "UPDATE batch_spec_resolution_jobs SET state = 'completed' WHERE batch_spec_id = $1",
        )
        .bind(spec.id)
        .execute(self.db.pool())
        .await
        .unwrap();
        BatchSpecWorkspaceStore::new(&self.db)
            .create_many(spec.id, &workspaces)
            .await
            .unwrap();
        spec
    }

    async fn execute(&self, spec: &BatchSpec, no_cache: Option<bool>) -> u64 {
        execute_batch_spec(
            &self.db,
            None,
            spec,
            ExecuteOptions {
                no_cache,
                auto_apply: false,
            },
        )
        .await
        .unwrap()
        .jobs
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        auth: &str,
    ) -> (u16, serde_json::Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .header("authorization", auth)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_default())
    }

    async fn call(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        self.post(path, body, &format!("token-executor {TOKEN}"))
            .await
    }

    async fn dequeue_with(&self, executor: &str, capabilities: &[&str]) -> Option<Job> {
        let req = DequeueRequest {
            executor_name: executor.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        };
        let (status, body) = self
            .call(
                "/.executors/queue/batches/dequeue",
                serde_json::to_value(req).unwrap(),
            )
            .await;
        match status {
            200 => Some(serde_json::from_value(body).unwrap()),
            204 => None,
            s => panic!("dequeue: {s} {body}"),
        }
    }

    async fn dequeue(&self, executor: &str) -> Option<Job> {
        self.dequeue_with(executor, BATCHES).await
    }

    async fn heartbeat(&self, executor: &str, ids: &[i64]) -> HeartbeatResponse {
        let req = HeartbeatRequest {
            executor_name: executor.into(),
            queue_names: vec!["batches".into()],
            jobs: vec![QueueJobIds {
                queue_name: "batches".into(),
                job_ids: ids.to_vec(),
            }],
            version: env!("CARGO_PKG_VERSION").into(),
            runtime: "docker".into(),
            ..Default::default()
        };
        let (status, body) = self
            .call(
                "/.executors/queue/heartbeat",
                serde_json::to_value(req).unwrap(),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn mark(&self, action: &str, executor: &str, job: i64) -> u16 {
        self.call(
            &format!("/.executors/queue/batches/{action}"),
            json!({"executorName": executor, "jobId": job, "errorMessage": "step 1 failed"}),
        )
        .await
        .0
    }

    async fn add_cache_entry(&self, job: &Job, key: &str, value: &str) -> (u16, serde_json::Value) {
        self.post(
            "/.executors/queue/batches/addCacheEntry",
            json!({"key": key, "value": value}),
            &format!("Bearer {}", job.token),
        )
        .await
    }

    async fn upload_result(&self, job: &Job, specs: &[String]) -> (u16, serde_json::Value) {
        self.post(
            "/.executors/queue/batches/uploadResult",
            json!({ "changesetSpecs": specs }),
            &format!("Bearer {}", job.token),
        )
        .await
    }

    async fn job_state(&self, id: i64) -> JobState {
        BatchSpecWorkspaceStore::new(&self.db)
            .execution_job(id)
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

/// The result of step `i` of [`SPEC`], as the step runner would cache it.
fn step_result(i: usize) -> AfterStepResult {
    AfterStepResult {
        version: sourcegull_batches_steps::result::RESULT_VERSION,
        step_index: i,
        diff: DIFF.as_bytes().to_vec(),
        changed_files: ChangedFiles {
            modified: vec!["README.md".into()],
            ..Default::default()
        },
        stdout: String::new(),
        stderr: String::new(),
        outputs: [("greeting".to_owned(), "hello".into())].into(),
    }
}

fn cached(i: usize) -> String {
    CachedStep {
        fingerprint: "f".repeat(64),
        result: step_result(i),
    }
    .to_json()
}

/// What the executor builds and uploads for a job: the changeset specs of the final result.
fn changeset_specs(job: &Job, result: &AfterStepResult) -> Vec<String> {
    let b = job.batches.as_ref().unwrap();
    let spec = sourcegull_batches::BatchSpec::parse(&b.raw_spec).unwrap();
    let author = b
        .fallback_author
        .as_ref()
        .map(|a| sourcegull_batches::spec::CommitAuthor {
            name: a.name.clone(),
            email: a.email.clone(),
        });
    let repo = SpecRepository {
        id: b.repository.id.clone(),
        name: b.repository.name.clone(),
        base_ref: b.repository.base_ref.clone(),
        base_rev: b.repository.base_rev.clone(),
        file_matches: b.repository.file_matches.clone(),
    };
    build_changeset_specs(&ChangesetSpecInput {
        spec: &spec,
        repository: &repo,
        path: &b.path,
        result,
        fallback_author: author.as_ref(),
    })
    .unwrap()
    .iter()
    .map(|s| s.to_json())
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dequeue_payload_and_secrets() {
    let Some(env) = setup().await else {
        return;
    };
    let store = env.db.executor_secrets(Some(env.keyring.clone()));
    let batches = ExecutorSecretScope::Batches;
    let mine = store
        .create(
            batches,
            "GITLAB_TOKEN",
            "alice-gitlab-token",
            Some(Namespace::User(env.alice)),
            Some(env.alice),
        )
        .await
        .unwrap();
    // Overwritten by alice's own; not requested; another scope; another user's.
    let global = store
        .create(batches, "GITLAB_TOKEN", "global-token", None, None)
        .await
        .unwrap();
    let unused = store
        .create(batches, "UNUSED", "unused-value", None, None)
        .await
        .unwrap();
    store
        .create(
            ExecutorSecretScope::CodeIntel,
            "GITLAB_TOKEN",
            "codeintel-token",
            None,
            None,
        )
        .await
        .unwrap();
    store
        .create(
            batches,
            "GITLAB_TOKEN",
            "bob-token",
            Some(Namespace::User(env.bob)),
            Some(env.bob),
        )
        .await
        .unwrap();
    let docker = store
        .create(
            batches,
            "DOCKER_AUTH_CONFIG",
            r#"{"auths": {"registry.example.com": {"auth": "dXNlcjpwYXNz"}}}"#,
            None,
            None,
        )
        .await
        .unwrap();

    let mut ws = env.workspace(env.repo);
    ws.path = "web".into();
    ws.only_fetch_workspace = true;
    ws.file_matches = vec!["web/README.md".into()];
    let spec = env.spec_with(env.alice, SPEC, vec![ws]).await;
    assert_eq!(env.execute(&spec, None).await, 1);

    // Executors without the batch step runner or secrets get nothing.
    assert!(
        env.dequeue_with("old", &[capability::DOCKER_STEPS, capability::SECRETS])
            .await
            .is_none()
    );
    assert!(
        env.dequeue_with("old", &[capability::BATCH_STEPS])
            .await
            .is_none()
    );

    let job = env.dequeue("exec-1").await.expect("job");
    assert_eq!(job.queue, "batches");
    assert_eq!(job.repository_name, REPO);
    assert_eq!(job.commit, COMMIT);
    assert!(job.shallow_clone);
    assert_eq!(job.sparse_checkout, ["web"]);
    assert!(job.docker_steps.is_empty() && job.upload.is_none());
    assert!(job.token.starts_with("sjt_"));
    assert_eq!(job.redacted_values[&job.token], "JOB_TOKEN_REMOVED");
    let b = job.batches.as_ref().expect("batches payload");
    assert_eq!(b.version, sourcegull_executor_api::BATCHES_JOB_VERSION);
    assert_eq!(b.raw_spec, SPEC);
    assert_eq!(
        sourcegull_batches::BatchSpec::parse(&b.raw_spec)
            .unwrap()
            .steps
            .len(),
        2
    );
    assert_eq!(b.repository.name, REPO);
    assert_eq!(b.repository.base_ref, "refs/heads/main");
    assert_eq!(b.repository.branch, "main");
    assert_eq!(b.repository.base_rev, COMMIT);
    assert_eq!(b.repository.file_matches, ["web/README.md"]);
    assert_eq!(
        b.repository.id,
        sourcegull_services::batches::repository_id(env.repo)
    );
    assert_eq!(b.path, "web");
    assert!(b.only_fetch_workspace && !b.no_cache);
    assert!(b.cached_steps.is_empty());
    let author = b.fallback_author.as_ref().expect("author");
    assert_eq!(
        (author.name.as_str(), author.email.as_str()),
        ("alice", "alice@example.com")
    );

    // Only the passed-through secret, from alice's namespace; values redacted.
    assert_eq!(job.secret_env, ["GITLAB_TOKEN=alice-gitlab-token"]);
    assert_eq!(
        job.redacted_values["alice-gitlab-token"],
        "${{ secrets.GITLAB_TOKEN }}"
    );
    assert_eq!(
        job.docker_auth_config.as_ref().unwrap().auths["registry.example.com"].auth,
        "dXNlcjpwYXNz"
    );
    // Each read is logged as alice.
    for id in [mine.id, docker.id] {
        let logs = store.access_logs(id, 10, 0).await.unwrap();
        assert_eq!(logs.len(), 1, "secret {id}");
        assert_eq!(logs[0].user_id, Some(env.alice));
        assert_eq!(logs[0].machine_user, "");
    }
    for id in [global.id, unused.id] {
        assert!(store.access_logs(id, 10, 0).await.unwrap().is_empty());
    }
    let events = env
        .db
        .security_events()
        .list_recent(None, 10)
        .await
        .unwrap();
    let read = events
        .iter()
        .find(|e| e.name == "ExecutorSecretRead")
        .expect("security event");
    assert_eq!(read.argument["queue"], "batches");
    assert_eq!(read.argument["jobId"], job.id);
    assert!(!read.argument.to_string().contains("alice-gitlab-token"));

    // The job token is good for this job's uploads only, and the executor token is not.
    let (status, _) = env
        .post(
            "/.executors/queue/batches/uploadResult",
            json!({"changesetSpecs": []}),
            &format!("token-executor {TOKEN}"),
        )
        .await;
    assert_eq!(status, 401);
    assert!(env.dequeue("exec-2").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_complete_and_cached_reexecution() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(env.alice, SPEC, &[env.repo]).await;
    assert_eq!(env.execute(&spec, None).await, 1);
    let job = env.dequeue("exec-1").await.unwrap();
    let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
    let ws = BatchSpecWorkspaceStore::new(&env.db)
        .list(&ListWorkspaces {
            batch_spec_id: spec.id,
            ..Default::default()
        })
        .await
        .unwrap()
        .remove(0);
    let keys = step_cache_keys(&parsed, REPO, &ws);
    assert_eq!(keys.len(), 2);

    // Step results: only the job's keys, and the value must be the key's step.
    let (status, body) = env.add_cache_entry(&job, &"0".repeat(64), &cached(0)).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = env.add_cache_entry(&job, &keys[1], &cached(0)).await;
    assert_eq!(status, 400, "{body}");
    let (status, _) = env.add_cache_entry(&job, &keys[0], "not json").await;
    assert_eq!(status, 400);
    for (i, key) in keys.iter().enumerate() {
        let (status, body) = env.add_cache_entry(&job, key, &cached(i)).await;
        assert_eq!(status, 204, "{body}");
    }
    let stored = ExecutionCacheStore::new(&env.db)
        .get_many(env.alice, &keys)
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);

    // Changeset specs: validated like createChangesetSpec, of the job's repository only.
    let specs = changeset_specs(&job, &step_result(1));
    assert_eq!(specs.len(), 1);
    let (status, _) = env.upload_result(&job, &["{}".to_owned()]).await;
    assert_eq!(status, 400);
    let foreign = specs[0].replace(
        &sourcegull_services::batches::repository_id(env.repo),
        &sourcegull_services::batches::repository_id(env.other),
    );
    let (status, body) = env.upload_result(&job, &[foreign]).await;
    assert_eq!(status, 400, "{body}");
    let import = json!({"baseRepository": sourcegull_services::batches::repository_id(env.repo), "externalID": "12"}).to_string();
    let (status, _) = env.upload_result(&job, &[import]).await;
    assert_eq!(status, 400);
    // Uploading twice replaces the first upload.
    for _ in 0..2 {
        let (status, body) = env.upload_result(&job, &specs).await;
        assert_eq!(status, 200, "{body}");
        let resp: BatchesResultResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.changeset_spec_ids.len(), 1);
    }
    let ws = BatchSpecWorkspaceStore::new(&env.db)
        .get(ws.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ws.changeset_spec_ids.len(), 1);
    let cs = ChangesetSpecStore::new(&env.db)
        .get(ws.changeset_spec_ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cs.batch_spec_id, Some(spec.id));
    assert_eq!(cs.user_id, Some(env.alice));
    assert_eq!(cs.head_ref.as_deref(), Some("refs/heads/hello"));
    assert_eq!(cs.commit_author_email.as_deref(), Some("alice@example.com"));
    assert_eq!(
        (cs.diff_stat_added, cs.diff_stat_deleted),
        (Some(1), Some(0))
    );

    assert_eq!(env.mark("markComplete", "exec-1", job.id).await, 204);
    assert_eq!(env.job_state(job.id).await, JobState::Completed);
    let stats = BatchSpecWorkspaceStore::new(&env.db)
        .stats(spec.id)
        .await
        .unwrap();
    assert_eq!((stats.executions, stats.completed), (1, 1));
    // The token is revoked with the job.
    let (status, _) = env.upload_result(&job, &specs).await;
    assert_eq!(status, 401);

    // A new spec on the same workspace finds every step in alice's cache (as the resolver
    // records it), so executing it builds the changeset spec without a job.
    let entries = ExecutionCacheStore::new(&env.db)
        .get_many(env.alice, &keys)
        .await
        .unwrap();
    let by_key: BTreeMap<&str, &str> = entries
        .iter()
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    let mut hit = env.workspace(env.repo);
    hit.cached_result_found = true;
    hit.step_cache_results = json!({
        "1": {"key": keys[0], "value": serde_json::from_str::<serde_json::Value>(by_key[keys[0].as_str()]).unwrap()},
        "2": {"key": keys[1], "value": serde_json::from_str::<serde_json::Value>(by_key[keys[1].as_str()]).unwrap()},
    });
    let again = env.spec_with(env.alice, SPEC, vec![hit.clone()]).await;
    assert_eq!(env.execute(&again, None).await, 0);
    let ws = &BatchSpecWorkspaceStore::new(&env.db)
        .list(&ListWorkspaces {
            batch_spec_id: again.id,
            ..Default::default()
        })
        .await
        .unwrap()[0];
    assert_eq!(ws.changeset_spec_ids.len(), 1);
    let built = ChangesetSpecStore::new(&env.db)
        .get(ws.changeset_spec_ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(built.diff.as_deref(), cs.diff.as_deref());
    assert_eq!(built.title.as_deref(), Some("Hello"));
    assert!(env.dequeue("exec-1").await.is_none());
    // Executing twice is refused.
    assert!(
        execute_batch_spec(&env.db, None, &again, ExecuteOptions::default())
            .await
            .is_err()
    );

    // With noCache the workspace runs, with no cached steps in the payload.
    let fresh = env.spec_with(env.alice, SPEC, vec![hit]).await;
    assert_eq!(env.execute(&fresh, Some(true)).await, 1);
    let job = env.dequeue("exec-1").await.unwrap();
    let b = job.batches.unwrap();
    assert!(b.no_cache && b.cached_steps.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partially_cached_workspace_resumes() {
    let Some(env) = setup().await else {
        return;
    };
    let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
    let mut ws = env.workspace(env.repo);
    let probe = env.spec_with(env.alice, SPEC, vec![ws.clone()]).await;
    let row = &BatchSpecWorkspaceStore::new(&env.db)
        .list(&ListWorkspaces {
            batch_spec_id: probe.id,
            ..Default::default()
        })
        .await
        .unwrap()[0];
    let keys = step_cache_keys(&parsed, REPO, row);
    ws.step_cache_results = json!({"1": {"key": keys[0], "value": serde_json::from_str::<serde_json::Value>(&cached(0)).unwrap()}});
    let spec = env.spec_with(env.alice, SPEC, vec![ws]).await;
    assert_eq!(env.execute(&spec, None).await, 1);
    // Fairness: the probe spec's workspace is not executed, so this is the only job.
    let job = env.dequeue("exec-1").await.unwrap();
    let b = job.batches.unwrap();
    assert_eq!(b.cached_steps.len(), 1);
    assert_eq!(b.cached_steps[0].step_index, 0);
    assert_eq!(b.cached_steps[0].key, keys[0]);
    let entry = CachedStep::from_json(&b.cached_steps[0].value).unwrap();
    assert_eq!(entry.result, step_result(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_through_heartbeat() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(env.alice, SPEC, &[env.repo, env.other]).await;
    assert_eq!(env.execute(&spec, None).await, 2);
    let job = env.dequeue("exec-1").await.unwrap();
    let hb = env.heartbeat("exec-1", &[job.id]).await;
    assert!(hb.cancel.is_empty());

    // The queued job is canceled at once, the running one at the next heartbeat.
    assert_eq!(
        BatchSpecWorkspaceStore::new(&env.db)
            .cancel_executions(spec.id)
            .await
            .unwrap(),
        2
    );
    let hb = env.heartbeat("exec-1", &[job.id]).await;
    assert_eq!(hb.cancel[0].job_ids, [job.id]);
    assert_eq!(hb.known[0].job_ids, [job.id]);
    // The executor stops and reports a failure, recorded as canceled.
    assert_eq!(env.mark("markFailed", "exec-1", job.id).await, 204);
    assert_eq!(env.job_state(job.id).await, JobState::Canceled);
    let stats = BatchSpecWorkspaceStore::new(&env.db)
        .stats(spec.id)
        .await
        .unwrap();
    assert_eq!((stats.canceled, stats.queued, stats.processing), (2, 0, 0));
    assert!(env.dequeue("exec-1").await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_and_errors() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(env.alice, SPEC, &[env.repo]).await;
    env.execute(&spec, None).await;
    let job = env.dequeue("exec-1").await.unwrap();
    // Only the holder may finish it.
    assert_eq!(env.mark("markFailed", "exec-2", job.id).await, 404);
    // Logs.
    let (status, body) = env
        .call(
            "/.executors/queue/batches/addExecutionLogEntry",
            json!({"executorName": "exec-1", "jobId": job.id,
                   "entry": {"key": "step.0", "command": ["alpine:3", "echo"], "out": "stdout: hi\n"}}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(env.mark("markErrored", "exec-1", job.id).await, 204);
    assert_eq!(env.job_state(job.id).await, JobState::Queued);
    // Retried after a backoff; skip it.
    sqlx::query("UPDATE batch_spec_workspace_execution_jobs SET process_after = NULL")
        .execute(env.db.pool())
        .await
        .unwrap();
    let job = env.dequeue("exec-1").await.unwrap();
    assert_eq!(env.mark("markFailed", "exec-1", job.id).await, 204);
    assert_eq!(env.job_state(job.id).await, JobState::Failed);
    assert_eq!(
        BatchSpecWorkspaceStore::new(&env.db)
            .failure_messages(spec.id, 10)
            .await
            .unwrap(),
        ["step 1 failed"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fair_dequeue_between_users() {
    let Some(env) = setup().await else {
        return;
    };
    let repos: Vec<RepoId> = vec![
        env.repo,
        env.other,
        insert_repo(&env.db, "gitlab.example.com/acme/third", "3").await,
    ];
    let a = env.spec(env.alice, SPEC, &repos).await;
    env.execute(&a, None).await;
    let b = env.spec(env.bob, SPEC, &[env.repo]).await;
    env.execute(&b, None).await;
    let mut users = Vec::new();
    while let Some(job) = env.dequeue("exec-1").await {
        let row = BatchSpecWorkspaceStore::new(&env.db)
            .execution_job(job.id)
            .await
            .unwrap()
            .unwrap();
        users.push(row.payload.user_id);
    }
    // Alice queued first, but bob's job does not wait for all of hers.
    assert_eq!(users, [env.alice, env.bob, env.alice, env.alice]);
}
