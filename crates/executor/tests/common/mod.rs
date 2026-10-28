//! A real server for executor tests: test Postgres, in-process repo node with a cloned
//! repository, filesystem blob store and the public HTTP API with an executor token, and
//! `CODEINTEL` executor secrets: [`SECRET_KEY`] and a `DOCKER_AUTH_CONFIG` for a registry
//! the tests do not pull from. For bulk changes ([`batches`]): the user [`Server::alice`]
//! with a `BATCHES` secret in her namespace, the GraphQL schema, and auto-apply.

// Tests insert repository rows directly.
#![allow(clippy::disallowed_methods, dead_code)]

pub mod batches;

use std::sync::Arc;
use std::time::{Duration, Instant};

use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_blobstore::{BlobStore, FsBlobStore};
use sourcegull_codeintel_scip::store::{CodeIntelStore, IndexJob, IndexJobRow, IndexJobStore};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ExecutorSecretScope, Namespace, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{BrowseRepoOids, RepoNode, browse_repo};
use sourcegull_types::{RepoId, UserId};
use tokio_util::sync::CancellationToken;

pub const REPO: &str = "gitlab.example.com/acme/demo";
pub const TOKEN: &str = "executor-secret-0123456789";
/// An executor secret the secret job requests.
pub const SECRET_KEY: &str = "SOURCEGULL_TEST_SECRET";
pub const SECRET_VALUE: &str = "very-secret-value-42";
/// The registry password of the `DOCKER_AUTH_CONFIG` secret (`bot:registry-pass-77`).
const REGISTRY_PASSWORD: &str = "registry-pass-77";

pub struct Server {
    pub db: Db,
    pub repo: RepoId,
    pub oids: BrowseRepoOids,
    /// The server's port.
    pub port: u16,
    pub blobs: Arc<dyn BlobStore>,
    pub keyring: Arc<Keyring>,
    /// The server state (with the auto-applier).
    pub state: AppState,
    /// The GraphQL schema of the server.
    pub schema: SourcegullSchema,
    /// A user with [`batches::SECRET_KEY`] as a `BATCHES` executor secret.
    pub alice: UserId,
    pub stop: CancellationToken,
    _t: TestDb,
    _node: RepoNode,
    _blob_dir: tempfile::TempDir,
}

impl Server {
    /// Starts the server listening on `bind` (e.g. `127.0.0.1`). `None` without a test
    /// database.
    pub async fn start(bind: &str) -> Option<Self> {
        let t = TestDb::new().await?;
        migrate::up(&t.pool).await.unwrap();
        let db = Db::from_pool(t.pool.clone());
        let (src, oids) = browse_repo();
        let node = RepoNode::start(src.remote()).await;
        let repo = RepoId(
            sourcegull_db::sqlx::query_scalar(
                "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
                 VALUES ($1, '1', 'gitlab', 'https://gitlab.example.com/') RETURNING id",
            )
            .bind(REPO)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        );
        node.clone_repo(repo, REPO).await;

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
        let blob_dir = tempfile::tempdir().unwrap();
        let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(blob_dir.path()).unwrap());
        let provider = Arc::new(StaticKeyProvider::new("k1", [2u8; 32]).unwrap());
        let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
        let secrets = db.executor_secrets(Some(keyring.clone()));
        secrets
            .create(
                ExecutorSecretScope::CodeIntel,
                SECRET_KEY,
                SECRET_VALUE,
                None,
                None,
            )
            .await
            .unwrap();
        secrets
            .create(
                ExecutorSecretScope::CodeIntel,
                "DOCKER_AUTH_CONFIG",
                r#"{"auths": {"registry.sourcegull.invalid": {"auth": "Ym90OnJlZ2lzdHJ5LXBhc3MtNzc="}}}"#,
                None,
                None,
            )
            .await
            .unwrap();
        let alice = db
            .users()
            .create(&NewUser {
                username: "alice".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id;
        db.user_emails()
            .add(alice, "alice@example.com")
            .await
            .unwrap();
        secrets
            .create(
                ExecutorSecretScope::Batches,
                batches::SECRET_KEY,
                batches::SECRET_VALUE,
                Some(Namespace::User(alice)),
                Some(alice),
            )
            .await
            .unwrap();
        let recorder = sourcegull_observability::build_recorder().unwrap();
        let state = sourcegull::features::install_auto_applier(
            AppState::new(
                db.clone(),
                config,
                MetricsHandle::from_prometheus(recorder.handle()),
                auth,
            )
            .with_repo_clients(node.clients.clone())
            .with_blobs(blobs.clone())
            .with_keyring(Some(keyring.clone())),
        );
        let schema = build_schema(Services::from_state(&state));
        let listener = tokio::net::TcpListener::bind(format!("{bind}:0"))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        tokio::spawn(sourcegull_http_api::serve(
            listener,
            public_router(state.clone()),
            stop.clone(),
        ));
        Some(Self {
            db,
            repo,
            oids,
            port,
            blobs,
            keyring,
            state,
            schema,
            alice,
            stop,
            _t: t,
            _node: node,
            _blob_dir: blob_dir,
        })
    }

    /// An index job running `local_steps` in `image` at `commit`.
    pub fn job(&self, commit: &str, image: &str, local_steps: &[&str]) -> IndexJob {
        IndexJob {
            repo_id: self.repo,
            commit: commit.to_owned(),
            root: String::new(),
            indexer: image.into(),
            indexer_args: Vec::new(),
            outfile: String::new(),
            docker_steps: Vec::new(),
            local_steps: local_steps.iter().map(|s| (*s).to_owned()).collect(),
            requested_env_vars: Vec::new(),
            should_reindex: false,
            enqueuer: None,
            associated_upload_id: None,
        }
    }

    /// A job that prints [`SECRET_KEY`], checks its value and writes an index.
    pub fn secret_job(&self, commit: &str, image: &str) -> IndexJob {
        let mut job = self.job(
            commit,
            image,
            &[
                &format!("echo \"secret is ${SECRET_KEY}\""),
                &format!("test \"${SECRET_KEY}\" = {SECRET_VALUE}"),
                "echo registry-pass-77 too",
                "printf scip-bytes > index.scip",
            ],
        );
        job.requested_env_vars = vec![SECRET_KEY.to_owned()];
        job
    }

    /// Checks the secret job: completed, the secret seen by the step but redacted in the log,
    /// and the reads in the access logs.
    pub async fn assert_secret_job(&self, row: &IndexJobRow) {
        assert_eq!(row.state, "completed", "{:?}", row.failure_message);
        let step = row
            .execution_logs
            .iter()
            .find(|e| e.key == "step.docker.indexer")
            .unwrap();
        assert!(
            step.out
                .contains(&format!("secret is ${{{{ secrets.{SECRET_KEY} }}}}")),
            "{}",
            step.out
        );
        assert!(
            step.out.contains("${{ secrets.DOCKER_AUTH_CONFIG }} too"),
            "{}",
            step.out
        );
        for entry in &row.execution_logs {
            assert!(
                !entry.out.contains(SECRET_VALUE),
                "secret in logs: {}",
                entry.out
            );
            assert!(!entry.out.contains(REGISTRY_PASSWORD));
        }
        let logs: Vec<(String, Option<i64>)> = sourcegull_db::sqlx::query_as(
            "SELECT l.machine_user, l.job_id FROM executor_secret_access_logs l
             JOIN executor_secrets s ON s.id = l.executor_secret_id WHERE s.key = $1",
        )
        .bind(SECRET_KEY)
        .fetch_all(self.db.pool())
        .await
        .unwrap();
        assert!(
            logs.contains(&("codeintel-autoindexing".to_owned(), Some(row.id))),
            "{logs:?}"
        );
    }

    /// Waits until every job has ended and returns their rows.
    pub async fn wait(&self, ids: &[i64], timeout: Duration) -> Vec<IndexJobRow> {
        let jobs = IndexJobStore::new(&self.db);
        let deadline = Instant::now() + timeout;
        loop {
            let mut rows = Vec::new();
            for id in ids {
                let row = jobs.get(*id).await.unwrap().unwrap();
                if matches!(row.state.as_str(), "completed" | "failed" | "errored") {
                    rows.push(row);
                }
            }
            if rows.len() == ids.len() {
                return rows;
            }
            assert!(Instant::now() < deadline, "jobs did not finish in time");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Checks the job that indexed successfully: logs, upload and blob.
    pub async fn assert_uploaded(&self, row: &IndexJobRow, keys: &[&str], index: &[u8]) {
        assert_eq!(row.state, "completed", "{:?}", row.failure_message);
        let logged: Vec<&str> = row.execution_logs.iter().map(|e| e.key.as_str()).collect();
        for key in keys {
            assert!(logged.contains(key), "{key} in {logged:?}");
        }
        let step = row
            .execution_logs
            .iter()
            .find(|e| e.key == "step.docker.indexer")
            .unwrap();
        assert_eq!(step.exit_code, Some(0));
        assert!(
            step.out.contains("indexing /data/repository"),
            "{}",
            step.out
        );
        assert!(step.duration_ms.is_some());
        let upload_id = row.job.associated_upload_id.expect("upload");
        let upload = CodeIntelStore::new(&self.db)
            .get_upload(upload_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(upload.associated_index_id, Some(row.id));
        assert_eq!(upload.commit, row.job.commit);
        let bytes = sourcegull_blobstore::get_bytes(
            &*self.blobs,
            &sourcegull_codeintel_scip::store::upload_index_key(upload_id),
        )
        .await
        .unwrap();
        assert_eq!(&bytes[..], index);
        for entry in &row.execution_logs {
            assert!(
                !entry.out.contains("sjt_"),
                "job token in logs: {}",
                entry.out
            );
        }
    }

    /// Checks a job whose step exited with status 3 after printing "about to fail".
    pub fn assert_step_failed(row: &IndexJobRow) {
        assert_eq!(row.state, "failed");
        assert_eq!(
            row.failure_message.as_deref(),
            Some("step step.docker.indexer exited with status 3")
        );
        let step = row
            .execution_logs
            .iter()
            .find(|e| e.key == "step.docker.indexer")
            .unwrap();
        assert_eq!(step.exit_code, Some(3));
        assert!(step.out.contains("about to fail"));
    }

    /// Checks that the executor called `name` reported itself with `runtime`.
    pub async fn assert_executor(&self, name: &str, runtime: &str) {
        let executors = self.db.executors().list(None).await.unwrap();
        assert!(
            executors
                .iter()
                .any(|e| e.hostname == name && e.runtime == runtime)
        );
    }
}
