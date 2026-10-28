//! Bulk change spec workspace files against the test Postgres over
//! HTTP: uploads as a command-line client's remote run sends them (multipart `filepath`, `filemod`,
//! `file`), the size limit, who may upload and download (creator, site admins, organization
//! members, nobody else), `batchChanges.enabled`, and executors downloading through
//! `/.executors/files/…` with a job token after the dequeued payload listed the files.

// Tests insert repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_batches::store::{
    BatchSpec, BatchSpecStore, BatchSpecWorkspaceFileStore, BatchSpecWorkspaceStore, NewBatchSpec,
    NewWorkspace,
};
use sourcegull_config::{ConfigHandle, SiteConfig, SiteConfigHandle};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_executor_api::{DequeueRequest, Job, capability};
use sourcegull_http_api::batch_files::parse_filemod;
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_services::batch_files::MAX_FILE_BYTES;
use sourcegull_services::batches::{ExecuteOptions, execute_batch_spec};
use sourcegull_services::relay::marshal_string_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, RepoId, UserId};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "executor-secret-0123456789";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const SPEC: &str = "version: 2
name: hello
on:
  - repository: gitlab.example.com/acme/demo
steps:
  - run: sh /tools/fix.sh
    container: alpine:3
    mount:
      - path: ./scripts/fix.sh
        mountpoint: /tools/fix.sh
changesetTemplate:
  title: Hello
  branch: hello
  commit:
    message: Say hello
";

struct Env {
    _t: TestDb,
    db: Db,
    config: SiteConfigHandle,
    base: String,
    http: reqwest::Client,
    repo: RepoId,
    alice: UserId,
    /// Access tokens (`Authorization` values).
    alice_auth: String,
    bob_auth: String,
    admin_auth: String,
    _cancel: tokio_util::sync::DropGuard,
}

async fn user(db: &Db, auth: &Authenticator, name: &str, admin: bool) -> (UserId, String) {
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
        sqlx::query("UPDATE users SET site_admin = true WHERE id = $1")
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
    (id, format!("token {token}"))
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let repo = RepoId(
        sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
             VALUES ('gitlab.example.com/acme/demo', '1', 'gitlab', 'https://gitlab.example.com/') \
             RETURNING id",
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
    );
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
    let (alice, alice_auth) = user(&db, &auth, "alice", false).await;
    let (_, bob_auth) = user(&db, &auth, "bob", false).await;
    let (_, admin_auth) = user(&db, &auth, "admin", true).await;
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    );
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
        config,
        base,
        http: reqwest::Client::new(),
        repo,
        alice,
        alice_auth,
        bob_auth,
        admin_auth,
        _cancel: cancel.drop_guard(),
    })
}

/// A multipart body exactly as Go's `mime/multipart` writes a client's upload.
fn multipart(filepath: &str, filemod: &str, name: &str, content: &[u8]) -> (String, Vec<u8>) {
    let boundary = "0d2a6a1e8f3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3";
    let mut body = Vec::new();
    for (field, value) in [("filepath", filepath), ("filemod", filemod)] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

impl Env {
    async fn spec(&self, ns: Namespace, from_raw: bool) -> BatchSpec {
        let parsed = sourcegull_batches::BatchSpec::parse(SPEC).unwrap();
        let spec = BatchSpecStore::new(&self.db)
            .create(&NewBatchSpec {
                raw_spec: SPEC.into(),
                spec: serde_json::to_value(&parsed).unwrap(),
                namespace: ns,
                user_id: Some(self.alice),
                created_from_raw: from_raw,
                allow_unsupported: false,
                allow_ignored: false,
                no_cache: false,
                batch_change_id: None,
            })
            .await
            .unwrap();
        if from_raw {
            BatchSpecStore::new(&self.db)
                .enqueue_resolution(spec.id, self.alice)
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
                .create_many(
                    spec.id,
                    &[NewWorkspace::new(self.repo, "refs/heads/main", COMMIT)],
                )
                .await
                .unwrap();
        }
        spec
    }

    async fn upload(
        &self,
        spec: &str,
        auth: Option<&str>,
        filepath: &str,
        name: &str,
        content: &[u8],
    ) -> (u16, serde_json::Value) {
        let (ct, body) = multipart(
            filepath,
            "2022-06-23 18:44:55.123456789 +0000 UTC",
            name,
            content,
        );
        let mut req = self
            .http
            .post(format!("{}/.api/files/batch-changes/{spec}", self.base))
            .header("content-type", ct)
            .body(body);
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or_default())
    }

    async fn get(&self, path: &str, auth: Option<&str>) -> (u16, Vec<u8>) {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.bytes().await.unwrap().to_vec())
    }
}

fn gql_id(spec: &BatchSpec) -> String {
    marshal_string_id("BatchSpec", &spec.rand_id)
}

#[test]
fn filemod_formats() {
    let t = parse_filemod("2022-06-23 18:44:55.123456789 +0000 UTC").unwrap();
    assert_eq!(t.to_rfc3339(), "2022-06-23T18:44:55.123456789+00:00");
    let t = parse_filemod("2022-06-23 20:44:55 +0200 CEST m=+0.000123").unwrap();
    assert_eq!(t.to_rfc3339(), "2022-06-23T18:44:55+00:00");
    assert!(parse_filemod("2022-06-23T18:44:55Z").is_some());
    assert!(parse_filemod("yesterday").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_and_download() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(Namespace::User(env.alice), true).await;
    let id = gql_id(&spec);

    // Anonymous, someone else, a bad directory, a local spec.
    let (status, _) = env.upload(&id, None, "scripts", "fix.sh", b"x").await;
    assert_eq!(status, 401);
    let (status, _) = env
        .upload(&id, Some(&env.bob_auth), "scripts", "fix.sh", b"x")
        .await;
    assert_eq!(status, 404);
    let (status, _) = env
        .upload(&id, Some(&env.alice_auth), "../up", "fix.sh", b"x")
        .await;
    assert_eq!(status, 400);
    let local = env.spec(Namespace::User(env.alice), false).await;
    let (status, _) = env
        .upload(&gql_id(&local), Some(&env.alice_auth), "", "fix.sh", b"x")
        .await;
    assert_eq!(status, 400);
    let (status, _) = env
        .upload(
            "QmF0Y2hTcGVjOiJub3BlIg==",
            Some(&env.alice_auth),
            "",
            "f",
            b"x",
        )
        .await;
    assert_eq!(status, 404);

    // The creator uploads (a client's "./scripts" becomes "scripts"); again replaces it.
    let (status, body) = env
        .upload(
            &id,
            Some(&env.alice_auth),
            "./scripts",
            "fix.sh",
            b"echo one\n",
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let file_id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = env
        .upload(
            &id,
            Some(&env.alice_auth),
            "scripts",
            "fix.sh",
            b"echo two\n",
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["id"], file_id.as_str());
    // A site admin may upload too; the random id works in the URL.
    let (status, _) = env
        .upload(
            &spec.rand_id,
            Some(&env.admin_auth),
            "",
            "README.md",
            b"# hi\n",
        )
        .await;
    assert_eq!(status, 200);
    let files = BatchSpecWorkspaceFileStore::new(&env.db)
        .list(spec.id, 0, 0)
        .await
        .unwrap();
    let names: Vec<String> = files.iter().map(|f| f.full_path()).collect();
    assert_eq!(names, ["README.md", "scripts/fix.sh"]);
    // Postgres keeps microseconds.
    assert_eq!(
        files[1].modified_at.to_rfc3339(),
        "2022-06-23T18:44:55.123456+00:00"
    );

    // Size limit: 10 MiB.
    let big = vec![b'a'; MAX_FILE_BYTES + 1];
    let (status, _) = env
        .upload(&id, Some(&env.alice_auth), "", "big.bin", &big)
        .await;
    assert_eq!(status, 413);
    let (status, _) = env
        .upload(
            &id,
            Some(&env.alice_auth),
            "",
            "max.bin",
            &big[..MAX_FILE_BYTES],
        )
        .await;
    assert_eq!(status, 200);

    // Downloads by id or by path; HEAD; others get 404, anonymous 401.
    let by_id = format!("/.api/files/batch-changes/{id}/{file_id}");
    let (status, body) = env.get(&by_id, Some(&env.alice_auth)).await;
    assert_eq!((status, body.as_slice()), (200, b"echo two\n".as_slice()));
    let (status, body) = env
        .get(
            &format!("/.api/files/batch-changes/{id}/scripts/fix.sh"),
            Some(&env.admin_auth),
        )
        .await;
    assert_eq!((status, body.as_slice()), (200, b"echo two\n".as_slice()));
    let head = env
        .http
        .head(format!("{}{by_id}", env.base))
        .header("authorization", &env.alice_auth)
        .send()
        .await
        .unwrap();
    assert_eq!(head.status().as_u16(), 200);
    assert_eq!(head.headers()["content-length"], "9");
    assert_eq!(env.get(&by_id, Some(&env.bob_auth)).await.0, 404);
    assert_eq!(env.get(&by_id, None).await.0, 401);
    assert_eq!(
        env.get(
            &format!("/.api/files/batch-changes/{id}/scripts/missing.sh"),
            Some(&env.alice_auth)
        )
        .await
        .0,
        404
    );

    // Bulk changes disabled.
    env.config.store(
        2,
        SiteConfig {
            batch_changes_enabled: false,
            executors_access_token: Some(TOKEN.into()),
            ..SiteConfig::default()
        },
    );
    assert_eq!(env.get(&by_id, Some(&env.alice_auth)).await.0, 403);
    let (status, _) = env.upload(&id, Some(&env.alice_auth), "", "x", b"x").await;
    assert_eq!(status, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn organization_members_download() {
    let Some(env) = setup().await else {
        return;
    };
    let org = env.db.orgs().create("acme", None).await.unwrap();
    let spec = env.spec(Namespace::Org(org.id), true).await;
    let id = gql_id(&spec);
    let (status, body) = env
        .upload(&id, Some(&env.alice_auth), "", "fix.sh", b"x")
        .await;
    assert_eq!(status, 200, "{body}");
    let path = format!("/.api/files/batch-changes/{id}/fix.sh");
    assert_eq!(env.get(&path, Some(&env.bob_auth)).await.0, 404);
    let bob = env
        .db
        .users()
        .get_by_username("bob")
        .await
        .unwrap()
        .unwrap()
        .id;
    env.db
        .orgs()
        .add_member(org.id, bob, OrgRole::Member)
        .await
        .unwrap();
    assert_eq!(env.get(&path, Some(&env.bob_auth)).await.0, 200);
    // Members may read, but only the creator uploads.
    let (status, _) = env
        .upload(&id, Some(&env.bob_auth), "", "fix.sh", b"y")
        .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executors_download_with_the_job_token() {
    let Some(env) = setup().await else {
        return;
    };
    let spec = env.spec(Namespace::User(env.alice), true).await;
    let id = gql_id(&spec);
    let (status, _) = env
        .upload(
            &id,
            Some(&env.alice_auth),
            "scripts",
            "fix.sh",
            b"echo fix\n",
        )
        .await;
    assert_eq!(status, 200);
    let other = env.spec(Namespace::User(env.alice), true).await;
    for s in [&spec, &other] {
        execute_batch_spec(
            &env.db,
            None,
            s,
            ExecuteOptions {
                no_cache: None,
                auto_apply: false,
            },
        )
        .await
        .unwrap();
    }
    // Executed: the files can no longer change.
    let (status, _) = env
        .upload(&id, Some(&env.alice_auth), "scripts", "fix.sh", b"changed")
        .await;
    assert_eq!(status, 409);

    let mut jobs = Vec::new();
    for _ in 0..2 {
        let resp = env
            .http
            .post(format!("{}/.executors/queue/batches/dequeue", env.base))
            .header("authorization", format!("token-executor {TOKEN}"))
            .json(&DequeueRequest {
                executor_name: "e1".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                capabilities: vec![capability::BATCH_STEPS.into(), capability::SECRETS.into()],
            })
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        jobs.push(resp.json::<Job>().await.unwrap());
    }
    let job = jobs
        .iter()
        .find(|j| j.batches.as_ref().unwrap().batch_spec_id == id)
        .unwrap();
    let other_job = jobs
        .iter()
        .find(|j| j.batches.as_ref().unwrap().batch_spec_id != id)
        .unwrap();
    assert!(
        other_job
            .batches
            .as_ref()
            .unwrap()
            .workspace_files
            .is_empty()
    );
    let files = &job.batches.as_ref().unwrap().workspace_files;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "scripts/fix.sh");
    assert_eq!(files[0].size, 9);
    assert!(
        files[0].url.starts_with(&format!(
            "/.executors/files/batch-changes/{}/",
            spec.rand_id
        )),
        "{}",
        files[0].url
    );

    let bearer = |j: &Job| format!("Bearer {}", j.token);
    let (status, body) = env.get(&files[0].url, Some(&bearer(job))).await;
    assert_eq!((status, body.as_slice()), (200, b"echo fix\n".as_slice()));
    // Another spec's job, no token, a user's token.
    assert_eq!(
        env.get(&files[0].url, Some(&bearer(other_job))).await.0,
        403
    );
    assert_eq!(env.get(&files[0].url, None).await.0, 401);
    assert_eq!(env.get(&files[0].url, Some(&env.alice_auth)).await.0, 401);
    // Once the job finished, its token is gone.
    let resp = env
        .http
        .post(format!(
            "{}/.executors/queue/batches/markComplete",
            env.base
        ))
        .header("authorization", format!("token-executor {TOKEN}"))
        .json(&serde_json::json!({"executorName": "e1", "jobId": job.id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    assert_eq!(env.get(&files[0].url, Some(&bearer(job))).await.0, 401);
}
