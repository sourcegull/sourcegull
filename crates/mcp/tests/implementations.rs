//! The `find_implementations` tool against the test Postgres, an in-process repo node and
//! SCIP uploads processed by the real upload worker: an interface in one repository with an
//! implementation there and one in another repository whose upload never references the
//! interface.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_blobstore::{Blobs, FsBlobStore};
use sourcegull_codeintel_scip::GraphCache;
use sourcegull_codeintel_scip::proto::{self, role};
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload, UploadState, upload_index_key};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const LIB: &str = "gitlab.example.com/acme/store";
const APP: &str = "gitlab.example.com/acme/app";

const STORE_GO: &str = "\
package store

type Store interface {
\tGet(key string) string
}

type memStore struct{}

func (memStore) Get(key string) string { return \"\" }
";

const DISK_GO: &str = "\
package app

type diskStore struct{}
";

const STORE: &str = "scip-go gomod example.com/store v1.0.0 `example.com/store`/Store#";
const STORE_GET: &str = "scip-go gomod example.com/store v1.0.0 `example.com/store`/Store#Get().";
const MEM: &str = "scip-go gomod example.com/store v1.0.0 `example.com/store`/memStore#";
const MEM_GET: &str = "scip-go gomod example.com/store v1.0.0 `example.com/store`/memStore#Get().";
const DISK: &str = "scip-go gomod example.com/app v1.0.0 `example.com/app`/diskStore#";

#[derive(Clone, PartialEq, prost::Message)]
struct Index {
    #[prost(message, optional, tag = "1")]
    metadata: Option<proto::Metadata>,
    #[prost(message, repeated, tag = "2")]
    documents: Vec<proto::Document>,
}

fn occ(range: [i32; 3], symbol: &str, roles: i32) -> proto::Occurrence {
    proto::Occurrence {
        range: range.to_vec(),
        symbol: symbol.into(),
        symbol_roles: roles,
        ..Default::default()
    }
}

fn info(symbol: &str, implements: Option<&str>) -> proto::SymbolInformation {
    proto::SymbolInformation {
        symbol: symbol.into(),
        relationships: implements
            .map(|s| proto::Relationship {
                symbol: s.into(),
                is_implementation: true,
                ..Default::default()
            })
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

fn index(
    path: &str,
    occurrences: Vec<proto::Occurrence>,
    symbols: Vec<proto::SymbolInformation>,
) -> Vec<u8> {
    Index {
        metadata: Some(proto::Metadata {
            tool_info: Some(proto::ToolInfo {
                name: "scip-go".into(),
                version: "1.0".into(),
                arguments: vec![],
            }),
            ..Default::default()
        }),
        documents: vec![proto::Document {
            language: "go".into(),
            relative_path: path.into(),
            position_encoding: proto::encoding::UTF8,
            occurrences,
            symbols,
        }],
    }
    .encode_to_vec()
}

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Blobs,
    url: String,
    http: reqwest::Client,
    token: String,
    commit: String,
    _cancel: tokio_util::sync::DropGuard,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let src = SourceRepo::new();
    let commit = src.commit("init", &[("store.go", STORE_GO), ("disk.go", DISK_GO)]);
    let node = RepoNode::start(src.remote()).await;
    for (i, name) in [LIB, APP].into_iter().enumerate() {
        let id: i32 = sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id, private) \
             VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/', false) RETURNING id",
        )
        .bind(name)
        .bind(i.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        node.clone_repo(RepoId(id), name).await;
    }

    let config = ConfigHandle::new(1, SiteConfig::default());
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let id = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let actor = auth.actor_for(id, AuthVia::Session).await.unwrap().unwrap();
    let (_, token) = auth
        .create_access_token(&actor, id, &["user:all".into()], "t", None)
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let blobs: Blobs = Arc::new(FsBlobStore::open(dir.path().join("blobs")).unwrap());
    let graphs =
        Arc::new(GraphCache::new(&dir.path().join("cache"), blobs.clone(), 1 << 30).unwrap());
    let (worker, resetter) = sourcegull_codeintel_scip::worker::upload_workers(
        &db,
        blobs.clone(),
        dir.path().join("work"),
        "test-worker",
        None,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(worker.run(cancel.clone()));
    tokio::spawn(resetter.run(cancel.clone()));

    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = sourcegull_services::AppState::new(
        db.clone(),
        config,
        sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_repo_clients(node.clients.clone())
    .with_blobs(blobs.clone())
    .with_graph_cache(graphs);
    let routes = sourcegull_http_api::public_routes().merge(sourcegull_mcp::routes());
    let router = sourcegull_http_api::finish(routes, state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        sourcegull_mcp::PATH
    );
    tokio::spawn(sourcegull_http_api::serve(listener, router, cancel.clone()));

    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        _dir: dir,
        db,
        blobs,
        url,
        http: reqwest::Client::new(),
        token,
        commit,
        _cancel: cancel.drop_guard(),
    })
}

impl Env {
    /// Uploads an index for the head of `repo` and waits until it is processed.
    async fn upload(&self, repo: &str, body: Vec<u8>) {
        let id: i32 = sourcegull_db::sqlx::query_scalar("SELECT id FROM repo WHERE name = $1")
            .bind(repo)
            .fetch_one(self.db.pool())
            .await
            .unwrap();
        let store = CodeIntelStore::new(&self.db);
        let id = store
            .create_upload(&NewUpload {
                repo_id: RepoId(id),
                commit: self.commit.clone(),
                root: String::new(),
                indexer: "scip-go".into(),
                indexer_version: String::new(),
                num_parts: 1,
                uploader: None,
                associated_index_id: None,
                ci_job: None,
            })
            .await
            .unwrap();
        let size = body.len() as i64;
        sourcegull_blobstore::put_bytes(&*self.blobs, &upload_index_key(id), body.into())
            .await
            .unwrap();
        assert!(store.mark_queued(id, size).await.unwrap());
        for _ in 0..200 {
            let u = store.get_upload(id).await.unwrap().unwrap();
            match u.state {
                UploadState::Completed => return,
                UploadState::Queued | UploadState::Processing => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                s => panic!("upload {id} is {s:?}: {:?}", u.failure_message),
            }
        }
        panic!("upload {id} was not processed");
    }

    /// Calls `find_implementations`; returns the text, asserting it is not an error.
    async fn implementations(&self, args: Value) -> String {
        let v: Value = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "find_implementations", "arguments": args },
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let r = &v["result"];
        assert_eq!(r["isError"], false, "{v}");
        r["content"][0]["text"].as_str().unwrap().to_owned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_implementations() {
    let Some(env) = setup().await else { return };

    // Without precise data the tool says so.
    let text = env
        .implementations(json!({ "repo": LIB, "path": "store.go", "line": 3, "symbol": "Store" }))
        .await;
    assert!(text.contains("No precise code intelligence data"), "{text}");

    env.upload(
        LIB,
        index(
            "store.go",
            vec![
                occ([2, 5, 10], STORE, role::DEFINITION),
                occ([3, 1, 4], STORE_GET, role::DEFINITION),
                occ([6, 5, 13], MEM, role::DEFINITION),
                occ([8, 6, 14], MEM, 0),
                occ([8, 16, 19], MEM_GET, role::DEFINITION),
            ],
            vec![
                info(STORE, None),
                info(STORE_GET, None),
                info(MEM, Some(STORE)),
                info(MEM_GET, Some(STORE_GET)),
            ],
        ),
    )
    .await;
    // The other repository's upload implements the interface without referencing it.
    env.upload(
        APP,
        index(
            "disk.go",
            vec![occ([2, 5, 14], DISK, role::DEFINITION)],
            vec![info(DISK, Some(STORE))],
        ),
    )
    .await;

    let text = env
        .implementations(json!({ "repo": LIB, "path": "store.go", "line": 3, "symbol": "Store" }))
        .await;
    assert!(
        text.starts_with(
            "Implementations 1–2 of `Store`: 2 implementations, 0 implemented symbols (precise)"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!("{LIB} store.go\n7:type memStore struct{{}}")),
        "{text}"
    );
    assert!(
        text.contains(&format!("{APP} disk.go\n3:type diskStore struct{{}}")),
        "{text}"
    );
    assert!(!text.contains("Store interface"), "{text}");

    // Paging.
    let text = env
        .implementations(
            json!({ "repo": LIB, "path": "store.go", "line": 3, "symbol": "Store", "limit": 1 }),
        )
        .await;
    assert!(
        text.contains("Implementations 1–1") && text.contains("more with offset=1"),
        "{text}"
    );
    assert!(
        text.contains("memStore") && !text.contains("diskStore"),
        "{text}"
    );
    let text = env
        .implementations(json!({
            "repo": LIB, "path": "store.go", "line": 3, "symbol": "Store", "limit": 1, "offset": 1,
        }))
        .await;
    assert!(
        text.contains("Implementations 2–2") && text.contains("diskStore"),
        "{text}"
    );
    assert!(!text.contains("more with offset"), "{text}");

    // Scope to the file's repository.
    let text = env
        .implementations(
            json!({ "repo": LIB, "path": "store.go", "line": 3, "symbol": "Store", "scope": "repo" }),
        )
        .await;
    assert!(text.contains("memStore") && !text.contains(APP), "{text}");

    // A method's implementations.
    let text = env
        .implementations(json!({ "repo": LIB, "path": "store.go", "line": 4, "symbol": "Get" }))
        .await;
    assert!(
        text.contains("9:func (memStore) Get(key string) string"),
        "{text}"
    );

    // A concrete type: the interface it implements.
    let text = env
        .implementations(
            json!({ "repo": LIB, "path": "store.go", "line": 7, "symbol": "memStore" }),
        )
        .await;
    assert!(
        text.contains("0 implementations, 1 implemented symbol"),
        "{text}"
    );
    assert!(
        text.contains("3:type Store interface {  [implements]"),
        "{text}"
    );
}
