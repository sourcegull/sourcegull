//! SCIP uploads (`/.api/scip/upload`) against the test Postgres and a filesystem
//! blob store: the endpoint, `gull code-intel upload` against it, and GitLab CI job tokens
//! checked against a mock GitLab. With `SOURCEGULL_TEST_S3_ENDPOINT` set (`scripts/dev test-s3`),
//! the blobs go to an S3 bucket (MinIO) instead.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_blobstore::{BlobStore, FsBlobStore};
use sourcegull_codeintel_scip::store::{CodeIntelStore, UploadCiJob, UploadState};
use sourcegull_config::{ConfigHandle, SiteConfig, SiteConfigHandle};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{Db, NewExternalService, migrate};
use sourcegull_http_api::public_router;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const REPO: &str = "gitlab.example.com/acme/demo";
const SECRET: &str = "gitlab.example.com/acme/secret";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Env {
    _t: TestDb,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Arc<dyn BlobStore>,
    router: Router,
    config: SiteConfigHandle,
    keyring: Arc<sourcegull_crypto::Keyring>,
    alice: String,
    bob: String,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    for (name, ext, private) in [(REPO, "1", false), (SECRET, "2", true)] {
        sqlx::query(
            "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id)
             VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/')",
        )
        .bind(name)
        .bind(private)
        .bind(ext)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let config = ConfigHandle::new(1, SiteConfig::default());
    let provider = Arc::new(StaticKeyProvider::new("k1", [1u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let dir = tempfile::tempdir().unwrap();
    // Upload ids restart in every test database: a fresh prefix keeps runs apart in the bucket.
    let prefix = format!("scip-upload-{}", rand_suffix());
    let blobs: Arc<dyn BlobStore> = match sourcegull_blobstore::testing::s3_from_env(&prefix).await
    {
        Some(s3) => Arc::new(s3),
        None => Arc::new(FsBlobStore::open(dir.path()).unwrap()),
    };
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth.clone(),
    )
    .with_blobs(blobs.clone())
    .with_keyring(Some(keyring.clone()));
    // The first user is a site admin; alice and bob are not.
    auth.sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap();
    let mut tokens = Vec::new();
    for name in ["alice", "bob"] {
        let user = sourcegull_db::NewUser {
            username: name.into(),
            ..Default::default()
        };
        let id = db.users().create(&user).await.unwrap().id;
        let actor = auth.actor_for(id, AuthVia::Session).await.unwrap().unwrap();
        let (_, secret) = auth
            .create_access_token(&actor, id, &["user:all".into()], "t", None)
            .await
            .unwrap();
        tokens.push(secret);
    }
    Some(Env {
        router: public_router(state),
        config,
        keyring,
        _t: t,
        _dir: dir,
        db,
        blobs,
        bob: tokens.pop().unwrap(),
        alice: tokens.pop().unwrap(),
    })
}

impl Env {
    async fn post(&self, query: &str, token: Option<&str>, body: &[u8]) -> (StatusCode, String) {
        self.post_auth(query, token.map(|t| format!("token {t}")), body)
            .await
    }

    async fn post_auth(
        &self,
        query: &str,
        authorization: Option<String>,
        body: &[u8],
    ) -> (StatusCode, String) {
        let mut req = Request::post(format!("/.api/scip/upload?{query}"))
            .header("content-type", "application/x-protobuf+scip");
        if let Some(a) = authorization {
            req = req.header("authorization", a);
        }
        let resp = self
            .router
            .clone()
            .oneshot(req.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn start(&self, repo: &str, extra: &str) -> String {
        format!(
            "repository={repo}&commit={COMMIT}&root=lib/&indexerName=scip-go&indexerVersion=v0.1{extra}"
        )
    }
}

fn rand_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos:x}-{}", std::process::id())
}

fn id_of(body: &str) -> i64 {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn single_and_multipart_uploads() {
    let Some(env) = setup().await else { return };
    let alice = Some(env.alice.as_str());

    let (status, _) = env.post(&env.start(REPO, ""), None, b"x").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // One request.
    let (status, body) = env
        .post(&env.start(REPO, ""), alice, b"gzipped index")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Queued);
    assert_eq!((u.commit.as_str(), u.root.as_str()), (COMMIT, "lib/"));
    assert_eq!(
        (u.indexer.as_str(), u.indexer_version.as_str()),
        ("scip-go", "v0.1")
    );
    assert_eq!(u.upload_size, Some(13));
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(blob, &b"gzipped index"[..]);

    // Multipart: parts in any order, done once all are there.
    let (status, body) = env
        .post(&env.start(REPO, "&multiPart=true&numParts=2"), alice, b"")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    let done = format!("uploadId={id}&done=true");
    let (status, body) = env.post(&done, alice, b"").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("[0, 1]"), "{body}");
    let (status, _) = env
        .post(&format!("uploadId={id}&index=1"), alice, b"world")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Another user cannot add to it.
    let (status, _) = env
        .post(&format!("uploadId={id}&index=0"), Some(&env.bob), b"evil ")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // No index means part 0, as clients may omit it.
    let (status, _) = env.post(&format!("uploadId={id}"), alice, b"hello ").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = env
        .post(&format!("uploadId={id}&index=2"), alice, b"x")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = env.post(&done, alice, b"").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Queued);
    assert_eq!(u.upload_size, Some(11));
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(blob, &b"hello world"[..]);
    assert_eq!(
        env.blobs.list(&format!("uploads/{id}/")).await.unwrap(),
        [format!("uploads/{id}/index")]
    );
    // Finished uploads take no more parts.
    let (status, _) = env
        .post(&format!("uploadId={id}&index=0"), alice, b"x")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Parts above the S3 store's part size (5 MiB in the S3 tests) are written as multipart
/// uploads, and so is the index they are joined into.
#[tokio::test]
async fn large_uploads() {
    let Some(env) = setup().await else { return };
    let alice = Some(env.alice.as_str());
    let part = |seed: u8| -> Vec<u8> {
        (0..6 * 1024 * 1024 + 7)
            .map(|i: usize| (i % 249) as u8 ^ seed)
            .collect()
    };
    let (a, b) = (part(1), part(2));

    let (status, body) = env.post(&env.start(REPO, ""), alice, &a).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    let key = format!("uploads/{id}/index");
    assert_eq!(env.blobs.size(&key).await.unwrap(), Some(a.len() as u64));
    assert_eq!(
        sourcegull_blobstore::get_bytes(&*env.blobs, &key)
            .await
            .unwrap(),
        a
    );

    let (status, body) = env
        .post(&env.start(REPO, "&multiPart=true&numParts=2"), alice, b"")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    for (i, data) in [&a, &b].into_iter().enumerate() {
        let (status, _) = env
            .post(&format!("uploadId={id}&index={i}"), alice, data)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, _) = env
        .post(&format!("uploadId={id}&done=true"), alice, b"")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.upload_size, Some((a.len() + b.len()) as i64));
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(blob, [a.as_slice(), b.as_slice()].concat());
    // A range of the joined index, across the boundary between the parts.
    let at = a.len() as u64;
    let range: Vec<bytes::Bytes> = futures::TryStreamExt::try_collect(
        env.blobs
            .get_range(&format!("uploads/{id}/index"), at - 3..at + 3)
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(range.concat(), [&a[a.len() - 3..], &b[..3]].concat());
}

#[tokio::test]
async fn rejected_uploads() {
    let Some(env) = setup().await else { return };
    let alice = Some(env.alice.as_str());
    for (query, why) in [
        (format!("commit={COMMIT}"), "no repository"),
        (format!("repository={REPO}&commit=abc"), "short commit"),
        (
            format!("repository={REPO}&commit={COMMIT}&root=../x"),
            "root outside",
        ),
        (
            format!("repository={REPO}&commit={COMMIT}&multiPart=true"),
            "no numParts",
        ),
    ] {
        let (status, body) = env.post(&query, alice, b"x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
    }
    // A repository alice cannot read answers like a missing one.
    let (status, hidden) = env.post(&env.start(SECRET, ""), alice, b"x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, missing) = env
        .post(&env.start("gitlab.example.com/acme/nope", ""), alice, b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        hidden.replace(SECRET, "X"),
        missing.replace("gitlab.example.com/acme/nope", "X")
    );
    let resp = env
        .router
        .clone()
        .oneshot(
            Request::post("/.api/lsif/upload")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::GONE);
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A SCIP index whose metadata names `scip-go v0.1.10`, padded with a "document" of
/// `noise` pseudo-random bytes (so gzip cannot shrink it much).
fn scip_index(noise: usize) -> Vec<u8> {
    let mut tool = Vec::new();
    put_bytes(&mut tool, 1, b"scip-go");
    put_bytes(&mut tool, 2, b"v0.1.10");
    let mut metadata = Vec::new();
    put_bytes(&mut metadata, 2, &tool);
    let mut out = Vec::new();
    put_bytes(&mut out, 1, &metadata);
    let mut x: u32 = 0x1234_5678;
    let doc: Vec<u8> = (0..noise)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    put_bytes(&mut out, 2, &doc);
    out
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(bytes), &mut out).unwrap();
    out
}

/// Serves the router on a local port; returns its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

fn vars(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

struct NoGit;

impl sourcegull_cli::codeintel::upload::Git for NoGit {
    fn run(&self, _: &[&str]) -> anyhow::Result<String> {
        anyhow::bail!("no git in this test")
    }
}

/// `gull code-intel upload` as a GitLab CI job would run it: values from `CI_*` and the index
/// metadata, then one request, or parts when the compressed index exceeds the payload limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gull_cli_uploads() {
    use sourcegull_cli::codeintel::transfer::{TransferOptions, upload};
    use sourcegull_cli::codeintel::upload::resolve;
    use sourcegull_cli::env::Instance;

    let Some(env) = setup().await else { return };
    let base = serve(env.router.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("lib")).unwrap();
    let index = scip_index(40_000);
    std::fs::write(dir.path().join("lib/index.scip"), &index).unwrap();
    let ci = vars(&[
        ("SOURCEGULL_ENDPOINT", &base),
        ("SOURCEGULL_ACCESS_TOKEN", &env.alice),
        ("GITLAB_CI", "true"),
        ("CI_PROJECT_URL", &format!("https://{REPO}")),
        ("CI_COMMIT_SHA", COMMIT),
        ("CI_PROJECT_DIR", dir.path().to_str().unwrap()),
        ("CI_JOB_TOKEN", "unused-with-an-access-token"),
    ]);
    let args = |extra: &[&str]| {
        let mut argv = vec!["gull", "code-intel", "upload", "-file=lib/index.scip"];
        argv.extend_from_slice(extra);
        use clap::Parser;
        match sourcegull_cli::Cli::parse_from(sourcegull_cli::args::normalize(argv)).command {
            sourcegull_cli::Command::CodeIntel {
                command: sourcegull_cli::CodeIntelCommand::Upload(a),
            } => a,
            other => panic!("{other:?}"),
        }
    };
    let resolved = resolve(&args(&[]), &ci, &NoGit, dir.path()).unwrap();
    assert_eq!(resolved.record.repo, REPO);
    assert_eq!(resolved.record.root, "lib");
    let instance = Instance::from_env(&ci).unwrap();
    let file = dir.path().join(&resolved.file);
    let quiet = |_: &str| {};

    // One request, compressed on the way.
    let id = upload(
        &instance,
        &resolved.record,
        &file,
        &TransferOptions::default(),
        &quiet,
    )
    .await
    .unwrap();
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Queued);
    assert_eq!((u.commit.as_str(), u.root.as_str()), (COMMIT, "lib/"));
    assert_eq!(
        (u.indexer.as_str(), u.indexer_version.as_str()),
        ("scip-go", "v0.1.10")
    );
    assert_eq!(u.num_parts, 1);
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(gunzip(&blob), index);

    // Parts of 7 kB, two at a time.
    let opts = TransferOptions {
        max_payload_size: 7_000,
        max_concurrency: 2,
        ..TransferOptions::default()
    };
    let id = upload(&instance, &resolved.record, &file, &opts, &quiet)
        .await
        .unwrap();
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Queued);
    assert!(u.num_parts >= 5, "{} parts", u.num_parts);
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(gunzip(&blob), index);

    // An already compressed index goes as it is.
    let gz = dir.path().join("lib/index.scip.gz");
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut enc, &index).unwrap();
    let compressed = enc.finish().unwrap();
    std::fs::write(&gz, &compressed).unwrap();
    let id = upload(&instance, &resolved.record, &gz, &opts, &quiet)
        .await
        .unwrap();
    let blob = sourcegull_blobstore::get_bytes(&*env.blobs, &format!("uploads/{id}/index"))
        .await
        .unwrap();
    assert_eq!(blob, compressed);

    // Client errors are not retried and carry the server's message.
    let mut bad = resolved.record.clone();
    bad.repo = "gitlab.example.com/acme/nope".into();
    let err = upload(&instance, &bad, &file, &TransferOptions::default(), &quiet)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
    assert!(err.to_string().contains("Unknown repository"), "{err}");
}

/// Uploads with a GitLab CI job token: accepted for the repository of the job's
/// project when `codeIntel.gitlabJobTokenUploads` is on; another project's repository, an
/// unknown token and a failing GitLab answer like a missing repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitlab_ci_job_token_uploads() {
    use sourcegull_codehost_gitlab::testing::MockGitLab;

    let Some(env) = setup().await else { return };
    let gitlab = MockGitLab::start("admin-token").await;
    gitlab.with(|s| {
        s.job_tokens.insert("job-demo".into(), 1);
        s.job_details.insert(
            "job-demo".into(),
            (501, "feature/x".into(), "dev-alice".into()),
        );
        s.job_tokens.insert("job-demo-2".into(), 1);
        s.job_tokens.insert("job-other".into(), 7);
    });
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: sourcegull_types::ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: serde_json::json!({ "url": gitlab.url, "token": "admin-token" }).to_string(),
        })
        .await
        .unwrap();
    // Both repositories (project 1 public, project 2 private) belong to the connection.
    sqlx::query(
        "INSERT INTO external_service_repos (external_service_id, repo_id, clone_url)
         SELECT $1, id, 'https://gitlab.example.com/' || name FROM repo",
    )
    .bind(svc.id)
    .execute(env.db.pool())
    .await
    .unwrap();
    let ci = |t: &str| Some(format!("token-gitlab-ci {t}"));

    // Off by default.
    let (status, body) = env
        .post_auth(&env.start(REPO, ""), ci("job-demo"), b"x")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("codeIntel.gitlabJobTokenUploads"), "{body}");
    assert!(gitlab.requests().iter().all(|r| r != "/api/v4/job"));

    env.config.store(
        2,
        SiteConfig {
            code_intel_gitlab_job_token_uploads: true,
            ..SiteConfig::default()
        },
    );
    // The job's project: accepted, single and multipart.
    let (status, body) = env
        .post_auth(&env.start(REPO, ""), ci("job-demo"), b"index")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id_of(&body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Queued);
    assert_eq!((u.uploader_user_id, u.associated_index_id), (None, None));
    assert_eq!(
        u.ci_job,
        Some(UploadCiJob {
            job_id: 501,
            git_ref: "feature/x".into(),
            username: "dev-alice".into(),
        })
    );
    let (status, body) = env
        .post_auth(
            &env.start(REPO, "&multiPart=true&numParts=1"),
            ci("job-demo"),
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    // Another project's job cannot add parts; a user cannot either.
    let (status, _) = env
        .post_auth(&format!("uploadId={id}&index=0"), ci("job-other"), b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .post(&format!("uploadId={id}&index=0"), Some(&env.alice), b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for query in [
        format!("uploadId={id}&index=0"),
        format!("uploadId={id}&done=true"),
    ] {
        let (status, body) = env.post_auth(&query, ci("job-demo"), b"part").await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{query}: {body}");
    }
    // Positive checks are cached: one GitLab request per token and repository.
    let job_requests = || {
        gitlab
            .requests()
            .iter()
            .filter(|r| *r == "/api/v4/job")
            .count()
    };
    assert_eq!(job_requests(), 2, "{:?}", gitlab.requests());

    // Another job of the same project cannot add parts to this job's upload.
    let (status, body) = env
        .post_auth(
            &env.start(REPO, "&multiPart=true&numParts=1"),
            ci("job-demo"),
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    for query in [
        format!("uploadId={id}&index=0"),
        format!("uploadId={id}&done=true"),
    ] {
        let (status, body) = env.post_auth(&query, ci("job-demo-2"), b"part").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query}: {body}");
    }
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.state, UploadState::Uploading);
    assert!(u.uploaded_parts.is_empty());

    // Rejections answer like a missing repository.
    let (_, missing) = env
        .post_auth(
            &env.start("gitlab.example.com/acme/nope", ""),
            ci("job-demo"),
            b"x",
        )
        .await;
    let same_as_missing = |name: &str, body: &str| {
        assert_eq!(
            body.replace(name, "X"),
            missing.replace("gitlab.example.com/acme/nope", "X")
        )
    };
    // Another project's job: the private repository looks missing.
    let (status, body) = env
        .post_auth(&env.start(SECRET, ""), ci("job-demo"), b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    same_as_missing(SECRET, &body);
    // A token GitLab does not know.
    let (status, body) = env
        .post_auth(&env.start(REPO, ""), ci("job-finished"), b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    same_as_missing(REPO, &body);
    // GitLab failing.
    gitlab.with(|s| s.job_error = true);
    let (status, body) = env
        .post_auth(&env.start(SECRET, ""), ci("job-other"), b"x")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    same_as_missing(SECRET, &body);
    gitlab.with(|s| {
        s.job_error = false;
        s.job_tokens.insert("job-secret".into(), 2);
    });
    // The private repository's own job can upload to it.
    let (status, body) = env
        .post_auth(&env.start(SECRET, ""), ci("job-secret"), b"x")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

/// `codeIntel.maxUploadBytes`: a single upload or the parts of a multipart upload going past
/// it answer 413, and the upload is deleted with its blobs.
#[tokio::test]
async fn upload_size_limit() {
    let Some(env) = setup().await else { return };
    env.config.store(
        2,
        SiteConfig {
            code_intel_max_upload_bytes: 10,
            ..SiteConfig::default()
        },
    );
    let alice = Some(env.alice.as_str());
    let rows = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM codeintel_uploads")
            .fetch_one(env.db.pool())
            .await
            .unwrap()
    };
    let blobs = || async { env.blobs.list("uploads/").await.unwrap() };

    // Single upload: one byte over.
    let (status, body) = env.post(&env.start(REPO, ""), alice, &[7u8; 11]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.contains("codeIntel.maxUploadBytes"), "{body}");
    assert_eq!(rows().await, 0);
    assert!(blobs().await.is_empty(), "{:?}", blobs().await);
    // At the limit: accepted.
    let (status, body) = env.post(&env.start(REPO, ""), alice, &[7u8; 10]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let kept = id_of(&body);

    // Multipart: the parts' total counts; a part sent again replaces the earlier one.
    let (status, body) = env
        .post(&env.start(REPO, "&multiPart=true&numParts=3"), alice, b"")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let id = id_of(&body);
    for (index, len) in [(0, 6), (0, 4), (1, 6)] {
        let (status, body) = env
            .post(
                &format!("uploadId={id}&index={index}"),
                alice,
                &vec![1u8; len],
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "part {index}: {body}");
    }
    let u = CodeIntelStore::new(&env.db)
        .get_upload(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.uploaded_bytes, 10);
    let (status, body) = env
        .post(&format!("uploadId={id}&index=2"), alice, b"x")
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(
        CodeIntelStore::new(&env.db)
            .get_upload(id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(rows().await, 1);
    let left = blobs().await;
    assert!(
        left.iter()
            .all(|k| k.starts_with(&format!("uploads/{kept}/"))),
        "{left:?}"
    );
    // The discarded upload is gone for its uploader too.
    let (status, _) = env
        .post(&format!("uploadId={id}&done=true"), alice, b"")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
