//! Upload processing through the queue against the test Postgres and a filesystem
//! blob store.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use prost::Message;
use sourcegull_blobstore::{BlobStore, FsBlobStore};
use sourcegull_codeintel_scip::CodeGraph;
use sourcegull_codeintel_scip::graph::posting;
use sourcegull_codeintel_scip::janitor::janitor_pass;
use sourcegull_codeintel_scip::proto;
use sourcegull_codeintel_scip::store::{
    CodeIntelStore, NewUpload, Upload, UploadState, upload_index_key,
};
use sourcegull_codeintel_scip::symbol::symbol_hash;
use sourcegull_codeintel_scip::worker::{CommitChecker, CommitStatus, upload_workers};
use sourcegull_db::{Db, migrate};
use sourcegull_testutil::TestDb;
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
/// A commit [`Commits`] does not know.
const UNKNOWN_COMMIT: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// Knows every commit but [`UNKNOWN_COMMIT`].
struct Commits;

#[async_trait::async_trait]
impl CommitChecker for Commits {
    async fn check(&self, _: RepoId, name: &str, commit: &str) -> CommitStatus {
        assert_eq!(name, "gitlab.example.com/acme/demo");
        if commit == UNKNOWN_COMMIT {
            CommitStatus::Missing
        } else {
            CommitStatus::Exists
        }
    }
}
const FOO: &str = "scip-go gomod example.com/demo v1.0.0 `example.com/demo`/Foo().";
const PRINTLN: &str = "scip-go gomod github.com/golang/go/src go1.22 fmt/Println().";

#[derive(Clone, PartialEq, prost::Message)]
struct Index {
    #[prost(message, optional, tag = "1")]
    metadata: Option<proto::Metadata>,
    #[prost(message, repeated, tag = "2")]
    documents: Vec<proto::Document>,
}

fn index() -> Vec<u8> {
    let occ = |range: Vec<i32>, symbol: &str, roles: i32| proto::Occurrence {
        range,
        symbol: symbol.into(),
        symbol_roles: roles,
        ..Default::default()
    };
    Index {
        metadata: Some(proto::Metadata {
            tool_info: Some(proto::ToolInfo {
                name: "scip-go".into(),
                version: "0.1.0".into(),
                arguments: vec![],
            }),
            ..Default::default()
        }),
        documents: vec![proto::Document {
            language: "go".into(),
            relative_path: "main.go".into(),
            occurrences: vec![
                occ(vec![2, 5, 8], FOO, proto::role::DEFINITION),
                occ(vec![3, 1, 8], PRINTLN, 0),
            ],
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

struct Env {
    _t: TestDb,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Arc<dyn BlobStore>,
    repo: RepoId,
    cancel: CancellationToken,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let repo: i32 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
         VALUES ('gitlab.example.com/acme/demo', '1', 'gitlab', 'https://gitlab.example.com/')
         RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(dir.path().join("blobs")).unwrap());
    let (worker, resetter) = upload_workers(
        &db,
        blobs.clone(),
        dir.path().join("work"),
        "test-worker",
        Some(Box::new(Commits)),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(worker.run(cancel.clone()));
    tokio::spawn(resetter.run(cancel.clone()));
    Some(Env {
        _t: t,
        _dir: dir,
        db,
        blobs,
        repo: RepoId(repo),
        cancel,
    })
}

impl Env {
    /// Uploads `body` (or no blob at all) and queues it.
    async fn upload(&self, indexer: &str, body: Option<Vec<u8>>) -> i64 {
        self.upload_at(COMMIT, indexer, body).await
    }

    async fn upload_at(&self, commit: &str, indexer: &str, body: Option<Vec<u8>>) -> i64 {
        let store = CodeIntelStore::new(&self.db);
        let id = store
            .create_upload(&NewUpload {
                repo_id: self.repo,
                commit: commit.into(),
                root: String::new(),
                indexer: indexer.into(),
                indexer_version: String::new(),
                num_parts: 1,
                uploader: None,
                associated_index_id: None,
                ci_job: None,
            })
            .await
            .unwrap();
        let size = match body {
            Some(b) => {
                sourcegull_blobstore::put_bytes(&*self.blobs, &upload_index_key(id), Bytes::from(b))
                    .await
                    .unwrap()
            }
            None => 1,
        };
        assert!(store.mark_queued(id, size as i64).await.unwrap());
        id
    }

    async fn wait(&self, id: i64) -> Upload {
        for _ in 0..200 {
            let u = CodeIntelStore::new(&self.db)
                .get_upload(id)
                .await
                .unwrap()
                .unwrap();
            if matches!(
                u.state,
                UploadState::Completed | UploadState::Failed | UploadState::Errored
            ) {
                return u;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("upload {id} was not processed");
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[tokio::test]
async fn processes_queued_uploads() {
    let Some(env) = setup().await else { return };
    let store = CodeIntelStore::new(&env.db);

    let first = env.upload("", Some(index())).await;
    let u = env.wait(first).await;
    assert_eq!(u.state, UploadState::Completed, "{:?}", u.failure_message);
    // The indexer comes from the index's metadata when the request named none.
    assert_eq!(u.indexer, "scip-go");
    assert_eq!(u.indexer_version, "0.1.0");
    assert_eq!(u.num_documents, Some(1));
    assert_eq!(u.num_occurrences, Some(2));
    assert_eq!(u.num_symbols, Some(2));
    let key = u.graph_key.clone().unwrap();
    let bytes = sourcegull_blobstore::get_bytes(&*env.blobs, &key)
        .await
        .unwrap();
    let graph = CodeGraph::from_vec(bytes.to_vec()).unwrap();
    let doc = graph.document("main.go").unwrap().unwrap();
    assert_eq!(doc.occurrences.len(), 2);

    let mut want = vec![
        (symbol_hash(FOO), i16::from(posting::DEFINITION)),
        (symbol_hash(PRINTLN), i16::from(posting::REFERENCE)),
    ];
    want.sort_unstable();
    assert_eq!(store.symbol_postings(first).await.unwrap(), want);
    let monikers: Vec<(String, String, String)> = store
        .package_monikers(first)
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.kind.to_owned(), m.name, m.version))
        .collect();
    assert_eq!(
        monikers,
        [
            ("export".into(), "example.com/demo".into(), "v1.0.0".into()),
            (
                "import".into(),
                "github.com/golang/go/src".into(),
                "go1.22".into()
            ),
        ]
    );

    // A newer upload of the same commit, root and indexer replaces the first.
    let second = env.upload("scip-go", Some(index())).await;
    assert_eq!(env.wait(second).await.state, UploadState::Completed);
    let old = store.get_upload(first).await.unwrap().unwrap();
    assert_eq!(old.state, UploadState::Deleting);

    // Reprocessing replaces the postings rather than adding to them.
    assert_eq!(store.requeue(&[second]).await.unwrap(), 1);
    assert_eq!(env.wait(second).await.state, UploadState::Completed);
    assert_eq!(store.symbol_postings(second).await.unwrap().len(), 2);
}

#[tokio::test]
async fn bad_uploads_fail_with_a_message() {
    let Some(env) = setup().await else { return };

    let lsif = env
        .upload("lsif-go", Some(br#"{"id":1,"type":"vertex"}"#.to_vec()))
        .await;
    let u = env.wait(lsif).await;
    assert_eq!(u.state, UploadState::Failed);
    assert!(
        u.failure_message
            .as_deref()
            .unwrap()
            .contains("not a SCIP protobuf"),
        "{:?}",
        u.failure_message
    );
    assert!(
        CodeIntelStore::new(&env.db)
            .symbol_postings(lsif)
            .await
            .unwrap()
            .is_empty()
    );

    let missing = env.upload("scip-go", None).await;
    let u = env.wait(missing).await;
    assert_eq!(u.state, UploadState::Failed);
    assert!(u.failure_message.unwrap().contains("missing"));
}

#[tokio::test]
async fn unknown_commits_are_retried_then_fail() {
    let Some(env) = setup().await else { return };
    let id = env
        .upload_at(UNKNOWN_COMMIT, "scip-go", Some(index()))
        .await;
    let mut u = None;
    for _ in 0..200 {
        let got = CodeIntelStore::new(&env.db)
            .get_upload(id)
            .await
            .unwrap()
            .unwrap();
        if got.failure_message.is_some() {
            u = Some(got);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let u = u.expect("the upload was not attempted");
    // Queued again with backoff (the repository may not have fetched the commit yet); the
    // last attempt fails it with the same message.
    assert_eq!(u.state, UploadState::Queued);
    assert_eq!(u.num_failures, 1);
    assert_eq!(
        u.failure_message.as_deref(),
        Some(format!("Commit {UNKNOWN_COMMIT} was not found in the repository.").as_str())
    );
    assert!(u.graph_key.is_none());
}

#[tokio::test]
async fn janitor_removes_deleted_uploads_and_orphans() {
    let Some(env) = setup().await else { return };
    let store = CodeIntelStore::new(&env.db);

    let first = env.upload("scip-go", Some(index())).await;
    assert_eq!(env.wait(first).await.state, UploadState::Completed);
    let second = env.upload("scip-go", Some(index())).await;
    assert_eq!(env.wait(second).await.state, UploadState::Completed);
    // `first` was overwritten.
    assert_eq!(
        store.get_upload(first).await.unwrap().unwrap().state,
        UploadState::Deleting
    );

    // An upload abandoned mid-way and a blob nothing refers to.
    let abandoned = store
        .create_upload(&NewUpload {
            repo_id: env.repo,
            commit: COMMIT.into(),
            root: String::new(),
            indexer: "scip-go".into(),
            indexer_version: String::new(),
            num_parts: 2,
            uploader: None,
            associated_index_id: None,
            ci_job: None,
        })
        .await
        .unwrap();
    sourcegull_blobstore::put_bytes(
        &*env.blobs,
        "uploads/999999/graph",
        Bytes::from_static(b"x"),
    )
    .await
    .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE codeintel_uploads SET uploaded_at = now() - interval '2 days' WHERE id = $1",
    )
    .bind(abandoned)
    .execute(env.db.pool())
    .await
    .unwrap();

    let report = janitor_pass(&env.db, &env.blobs).await.unwrap();
    assert_eq!(report.abandoned, 1);
    assert_eq!(report.deleted, 2);
    assert_eq!(report.orphaned_blobs, 1);

    for id in [first, abandoned] {
        let u = store.get_upload(id).await.unwrap().unwrap();
        assert_eq!(u.state, UploadState::Deleted);
        assert!(u.graph_key.is_none());
    }
    assert!(store.symbol_postings(first).await.unwrap().is_empty());
    assert!(store.package_monikers(first).await.unwrap().is_empty());
    let mut left = env.blobs.list("uploads/").await.unwrap();
    left.sort();
    assert_eq!(
        left,
        [
            format!("uploads/{second}/graph"),
            format!("uploads/{second}/index")
        ]
    );
    assert_eq!(store.symbol_postings(second).await.unwrap().len(), 2);

    // Nothing left to do.
    let again = janitor_pass(&env.db, &env.blobs).await.unwrap();
    assert_eq!(again, Default::default());
}
