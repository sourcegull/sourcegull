//! The executor API (`/.executors/*`) against the test Postgres, a real repo node and a
//! filesystem blob store: the queue protocol, job tokens, `git fetch` over the smart HTTP
//! endpoint and job-token uploads.

// Tests insert repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_blobstore::{BlobStore, FsBlobStore};
use sourcegull_codeintel_scip::store::{CodeIntelStore, IndexJob, IndexJobStore};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ExecutorSecretScope, Namespace, NewUser, migrate};
use sourcegull_executor_api::{
    DequeueRequest, HeartbeatRequest, HeartbeatResponse, Job, QueueJobIds, capability,
};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo, browse_repo};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const REPO: &str = "gitlab.example.com/acme/demo";
const OTHER: &str = "gitlab.example.com/acme/other";
const TOKEN: &str = "executor-secret-0123456789";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Arc<dyn BlobStore>,
    base: String,
    http: reqwest::Client,
    repo: RepoId,
    head: String,
    keyring: Arc<Keyring>,
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

async fn setup(token: Option<&str>) -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let (src, oids) = browse_repo();
    let node = RepoNode::start(src.remote()).await;
    let repo = insert_repo(&db, REPO, "1").await;
    let other = insert_repo(&db, OTHER, "2").await;
    node.clone_repo(repo, REPO).await;
    node.clone_repo(other, OTHER).await;
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            executors_access_token: token.map(str::to_owned),
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let dir = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(dir.path()).unwrap());
    let provider = Arc::new(StaticKeyProvider::new("k1", [6u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config,
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone())
    .with_blobs(blobs.clone())
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
        _src: src,
        _node: node,
        _dir: dir,
        db,
        blobs,
        base,
        http: reqwest::Client::new(),
        repo,
        head: oids.c3,
        keyring,
        _cancel: cancel.drop_guard(),
    })
}

impl Env {
    async fn call(
        &self,
        path: &str,
        body: serde_json::Value,
        token: &str,
    ) -> (u16, serde_json::Value) {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("token-executor {token}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_default())
    }

    async fn dequeue(&self, executor: &str) -> Option<Job> {
        self.dequeue_with(
            executor,
            &[
                capability::DOCKER_STEPS,
                capability::SCIP_UPLOAD,
                capability::SECRETS,
            ],
        )
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
                "/.executors/queue/codeintel/dequeue",
                serde_json::to_value(req).unwrap(),
                TOKEN,
            )
            .await;
        match status {
            200 => Some(serde_json::from_value(body).unwrap()),
            204 => None,
            s => panic!("dequeue: {s} {body}"),
        }
    }

    async fn heartbeat(&self, executor: &str, ids: &[i64]) -> HeartbeatResponse {
        let req = HeartbeatRequest {
            executor_name: executor.into(),
            queue_names: vec!["codeintel".into()],
            jobs: vec![QueueJobIds {
                queue_name: "codeintel".into(),
                job_ids: ids.to_vec(),
            }],
            os: "linux".into(),
            version: "0.1.0".into(),
            runtime: "docker".into(),
            ..Default::default()
        };
        let (status, body) = self
            .call(
                "/.executors/queue/heartbeat",
                serde_json::to_value(req).unwrap(),
                TOKEN,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn enqueue(&self) -> i64 {
        self.enqueue_with(&[]).await
    }

    async fn enqueue_with(&self, requested_env_vars: &[&str]) -> i64 {
        IndexJobStore::new(&self.db)
            .enqueue(&IndexJob {
                repo_id: self.repo,
                commit: self.head.clone(),
                root: String::new(),
                indexer: "sourcegraph/scip-go:latest".into(),
                indexer_args: vec!["scip-go".into()],
                outfile: String::new(),
                docker_steps: Vec::new(),
                local_steps: Vec::new(),
                requested_env_vars: requested_env_vars.iter().map(|v| (*v).to_owned()).collect(),
                should_reindex: false,
                enqueuer: None,
                associated_upload_id: None,
            })
            .await
            .unwrap()
    }

    /// `git fetch --depth=1` of `commit` from `/.executors/git/{repo}` into a new repository.
    fn fetch(&self, repo: &str, commit: &str, token: &str) -> (bool, tempfile::TempDir) {
        self.fetch_with(repo, commit, token, "2")
    }

    fn fetch_with(
        &self,
        repo: &str,
        commit: &str,
        token: &str,
        protocol: &str,
    ) -> (bool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let url = format!("{}/.executors/git/{repo}", self.base);
        let header = format!("http.extraHeader=Authorization: Bearer {token}");
        let version = format!("protocol.version={protocol}");
        let out = git(&[
            "-c",
            &header,
            "-c",
            &version,
            "fetch",
            "-q",
            "--depth=1",
            "--no-tags",
            &url,
            commit,
        ]);
        if !out.status.success() {
            return (false, dir);
        }
        assert!(git(&["checkout", "-q", "FETCH_HEAD"]).status.success());
        (true, dir)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_protocol_git_checkout_and_upload() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    // Bad tokens and unknown queues.
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/dequeue",
            json!({}),
            "wrong-token-0123456789",
        )
        .await;
    assert_eq!(status, 401);
    let (status, _) = env
        .call("/.executors/queue/nope/dequeue", json!({}), TOKEN)
        .await;
    assert_eq!(status, 404);
    assert!(env.dequeue("exec-1").await.is_none());

    let id = env.enqueue().await;
    // An executor without the capabilities gets nothing; a far older one is refused.
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/dequeue",
            json!({"executorName": "exec-0", "version": env!("CARGO_PKG_VERSION")}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 204);
    let (status, body) = env
        .call(
            "/.executors/queue/codeintel/dequeue",
            json!({"executorName": "exec-0", "version": "99.0.0", "capabilities": ["docker-steps", "scip-upload"]}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 409, "{body}");

    let job = env.dequeue("exec-1").await.expect("job");
    assert_eq!(job.id, id);
    assert_eq!(job.repository_name, REPO);
    assert_eq!(job.commit, env.head);
    // The annotated tag on the commit, peeled; tags of other commits are left out.
    assert_eq!(job.commit_tags, ["v1.0"]);
    assert_eq!(job.docker_steps.len(), 1);
    assert_eq!(job.docker_steps[0].image, "sourcegraph/scip-go:latest");
    assert_eq!(job.upload.as_ref().unwrap().file, "index.scip");
    assert!(job.token.starts_with("sjt_"));
    assert!(env.dequeue("exec-2").await.is_none());

    // Heartbeats: known jobs, the executor row, cancellation.
    let hb = env.heartbeat("exec-1", &[id]).await;
    assert_eq!(hb.known[0].job_ids, vec![id]);
    assert!(hb.cancel.is_empty());
    let executors = env.db.executors().list(None).await.unwrap();
    assert_eq!(executors.len(), 1);
    assert_eq!(executors[0].hostname, "exec-1");
    assert_eq!(executors[0].active_jobs[0].id, id);
    assert_eq!(executors[0].runtime, "docker");

    // Logs: only the holder may write them.
    let entry = json!({"key": "step.setup", "command": ["git", "fetch"], "out": "start"});
    let (status, body) = env
        .call(
            "/.executors/queue/codeintel/addExecutionLogEntry",
            json!({"executorName": "exec-1", "jobId": id, "entry": entry}),
            TOKEN,
        )
        .await;
    assert_eq!((status, body["index"].as_i64()), (200, Some(0)));
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/updateExecutionLogEntry",
            json!({"executorName": "exec-1", "jobId": id, "index": 0,
                   "entry": {"key": "step.setup", "out": "start\ndone", "exitCode": 0}}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/addExecutionLogEntry",
            json!({"executorName": "exec-2", "jobId": id, "entry": entry}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 404);
    let row = IndexJobStore::new(&env.db).get(id).await.unwrap().unwrap();
    assert_eq!(row.execution_logs[0].out, "start\ndone");
    assert_eq!(row.execution_logs[0].exit_code, Some(0));

    // Git checkout with the job token, for the job's repository only.
    let (ok, dir) = env.fetch(REPO, &env.head, &job.token);
    assert!(ok, "fetch with the job token");
    assert!(dir.path().join("docs/more.md").is_file());
    let (ok, dir) = env.fetch_with(REPO, &env.head, &job.token, "0");
    assert!(ok, "fetch over protocol v0");
    assert!(dir.path().join("docs/more.md").is_file());
    assert!(
        !env.fetch(OTHER, &env.head, &job.token).0,
        "other repository"
    );
    assert!(!env.fetch(REPO, &env.head, "sjt_nope").0, "unknown token");
    let resp = env
        .http
        .get(format!(
            "{}/.executors/git/{REPO}/info/refs?service=git-receive-pack",
            env.base
        ))
        .bearer_auth(&job.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403, "push is not served");

    // Upload with the job token: associated with the job, both ways.
    let upload = |repo: &'static str, token: String| {
        let url = format!(
            "{}/.api/scip/upload?repository={repo}&commit={}",
            env.base, env.head
        );
        let http = env.http.clone();
        async move {
            let resp = http
                .post(url)
                .bearer_auth(token)
                .body("index bytes")
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (
                status,
                resp.json::<serde_json::Value>().await.unwrap_or_default(),
            )
        }
    };
    assert_eq!(upload(OTHER, job.token.clone()).await.0, 404);
    let (status, body) = upload(REPO, job.token.clone()).await;
    assert_eq!(status, 202, "{body}");
    let upload_id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    let up = CodeIntelStore::new(&env.db)
        .get_upload(upload_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (up.associated_index_id, up.uploader_user_id),
        (Some(id), None)
    );
    let row = IndexJobStore::new(&env.db).get(id).await.unwrap().unwrap();
    assert_eq!(row.job.associated_upload_id, Some(upload_id));
    assert_eq!(
        env.blobs
            .size(&sourcegull_codeintel_scip::store::upload_index_key(
                upload_id
            ))
            .await
            .unwrap(),
        Some(11)
    );

    // Completing revokes the token.
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/markComplete",
            json!({"executorName": "exec-2", "jobId": id}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 404, "not the holder");
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/markComplete",
            json!({"executorName": "exec-1", "jobId": id}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 204);
    assert_eq!(
        IndexJobStore::new(&env.db)
            .get(id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "completed"
    );
    assert!(!env.fetch(REPO, &env.head, &job.token).0, "revoked token");
    assert_eq!(upload(REPO, job.token.clone()).await.0, 401);

    // Errors retry; a job the executor stops reporting is reclaimed after the grace period.
    let second = env.enqueue().await;
    let job = env.dequeue("exec-1").await.unwrap();
    assert_eq!(job.id, second);
    let (status, _) = env
        .call(
            "/.executors/queue/codeintel/markErrored",
            json!({"executorName": "exec-1", "jobId": second, "errorMessage": "indexer exited 1"}),
            TOKEN,
        )
        .await;
    assert_eq!(status, 204);
    let row = IndexJobStore::new(&env.db)
        .get(second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state.as_str(), row.failure_message.as_deref()),
        ("queued", Some("indexer exited 1"))
    );

    let queue = sourcegull_pgqueue::Queue::<IndexJob>::new(
        env.db.pool().clone(),
        sourcegull_codeintel_scip::store::IndexJobStore::queue_config(),
    )
    .unwrap();
    let third = env.enqueue().await;
    sqlx::query("UPDATE codeintel_index_jobs SET process_after = NULL WHERE id = $1")
        .bind(second)
        .execute(env.db.pool())
        .await
        .unwrap();
    queue.cancel(second).await.unwrap();
    let job = env.dequeue("exec-1").await.unwrap();
    assert_eq!(job.id, third);
    let hb = env.heartbeat("exec-1", &[third]).await;
    assert_eq!(hb.known[0].job_ids, vec![third]);
    // Pretend the job started long ago, then report nothing: it is reset.
    sqlx::query(
        "UPDATE codeintel_index_jobs SET started_at = now() - interval '5 minutes' WHERE id = $1",
    )
    .bind(third)
    .execute(env.db.pool())
    .await
    .unwrap();
    let hb = env.heartbeat("exec-1", &[]).await;
    assert!(hb.known[0].job_ids.is_empty());
    let row = IndexJobStore::new(&env.db)
        .get(third)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "queued");

    // Cancellation reaches the executor through the heartbeat.
    let job = env.dequeue("exec-1").await.unwrap();
    assert_eq!(job.id, third);
    queue.cancel(third).await.unwrap();
    let hb = env.heartbeat("exec-1", &[third]).await;
    assert_eq!(hb.cancel[0].job_ids, vec![third]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_without_an_access_token() {
    let Some(env) = setup(None).await else {
        return;
    };
    let (status, body) = env
        .call("/.executors/queue/codeintel/dequeue", json!({}), TOKEN)
        .await;
    assert_eq!(status, 503, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dequeue_resolves_executor_secrets() {
    let Some(env) = setup(Some(TOKEN)).await else {
        return;
    };
    let alice = env
        .db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let store = env.db.executor_secrets(Some(env.keyring.clone()));
    let codeintel = ExecutorSecretScope::CodeIntel;
    let one = store
        .create(codeintel, "SECRET_ONE", "first-secret-value", None, None)
        .await
        .unwrap();
    // `user:pass`
    let docker = store
        .create(
            codeintel,
            "DOCKER_AUTH_CONFIG",
            r#"{"auths": {"registry.example.com": {"auth": "dXNlcjpwYXNz"}}}"#,
            None,
            None,
        )
        .await
        .unwrap();
    // Neither another scope nor a user's namespace is visible to auto-indexing.
    let batches = store
        .create(
            ExecutorSecretScope::Batches,
            "SECRET_TWO",
            "batches-secret-value",
            None,
            None,
        )
        .await
        .unwrap();
    store
        .create(
            codeintel,
            "SECRET_THREE",
            "user-secret-value",
            Some(Namespace::User(alice)),
            Some(alice),
        )
        .await
        .unwrap();

    let with_secrets = env
        .enqueue_with(&["SECRET_ONE", "SECRET_TWO", "SECRET_THREE", "MISSING"])
        .await;
    let plain = env.enqueue().await;

    // An executor that cannot inject secrets only gets the job that needs none.
    let old = [capability::DOCKER_STEPS, capability::SCIP_UPLOAD];
    let job = env.dequeue_with("exec-old", &old).await.unwrap();
    assert_eq!(job.id, plain);
    assert!(job.secret_env.is_empty());
    assert!(job.docker_auth_config.is_none());
    assert!(env.dequeue_with("exec-old", &old).await.is_none());
    assert!(
        store
            .access_logs(docker.id, 10, 0)
            .await
            .unwrap()
            .is_empty(),
        "nothing read for it"
    );

    let job = env.dequeue("exec-new").await.unwrap();
    assert_eq!(job.id, with_secrets);
    assert_eq!(
        job.secret_env,
        vec!["SECRET_ONE=first-secret-value".to_owned()]
    );
    assert_eq!(
        job.redacted_values["first-secret-value"],
        "${{ secrets.SECRET_ONE }}"
    );
    assert!(job.docker_steps.iter().all(|s| s.env.is_empty()));
    let auth = job.docker_auth_config.expect("registry credentials");
    assert_eq!(auth.auths["registry.example.com"].auth, "dXNlcjpwYXNz");
    assert!(job.redacted_values.contains_key("dXNlcjpwYXNz"));

    // Every read is logged in the name of auto-indexing, with the job.
    for id in [one.id, docker.id] {
        let logs = store.access_logs(id, 10, 0).await.unwrap();
        assert_eq!(logs.len(), 1, "secret {id}");
        assert_eq!(logs[0].machine_user, "codeintel-autoindexing");
        assert_eq!(logs[0].user_id, None);
        assert_eq!(logs[0].job_id, Some(with_secrets));
    }
    assert!(
        store
            .access_logs(batches.id, 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
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
    assert_eq!(read.argument["jobId"], with_secrets);
    assert_eq!(read.argument["secrets"].as_array().unwrap().len(), 2);
    assert!(!read.argument.to_string().contains("first-secret-value"));
}
