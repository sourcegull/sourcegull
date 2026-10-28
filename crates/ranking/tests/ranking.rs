//! Ranking from the code graph: the reference graph exported from real code graph
//! files, and ranking passes against the test Postgres, a repo node started in-process over a
//! temp git repo, and SCIP uploads processed by the real upload worker.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use sourcegull_blobstore::{Blobs, FsBlobStore};
use sourcegull_codeintel_scip::proto::{self, role};
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload, UploadState, upload_index_key};
use sourcegull_codeintel_scip::symbol::symbol_hash;
use sourcegull_codeintel_scip::{CodeGraph, GraphCache, process_index};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_ranking::store::RankingStore;
use sourcegull_ranking::{Options, PassOutcome, Ranker, compute_ranks, export_upload};
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const CALC: &str = "gitlab.example.com/acme/calc";
const APP: &str = "gitlab.example.com/acme/app";

const ADD: &str = "rust-analyzer cargo calc 1.0.0 add().";
const HELP: &str = "rust-analyzer cargo calc 1.0.0 util/help().";

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

fn doc(path: &str, occurrences: Vec<proto::Occurrence>) -> proto::Document {
    proto::Document {
        language: "rust".into(),
        relative_path: path.into(),
        position_encoding: proto::encoding::UTF8,
        occurrences,
        symbols: Vec::new(),
    }
}

fn index(documents: Vec<proto::Document>) -> Vec<u8> {
    Index {
        metadata: Some(proto::Metadata {
            tool_info: Some(proto::ToolInfo {
                name: "rust-analyzer".into(),
                version: "1.0".into(),
                arguments: vec![],
            }),
            ..Default::default()
        }),
        documents,
    }
    .encode_to_vec()
}

/// The defining repository: `lib.rs` defines `add` (used by `main.rs` and `util.rs`),
/// `util.rs` defines `help` (used by `lib.rs`), `README.md` has no references.
fn calc_index() -> Vec<u8> {
    index(vec![
        doc(
            "src/lib.rs",
            vec![
                occ([1, 7, 10], ADD, role::DEFINITION),
                occ([1, 11, 12], "local 0", role::DEFINITION),
                occ([2, 4, 5], "local 0", 0),
                occ([3, 4, 8], HELP, 0),
            ],
        ),
        doc(
            "src/main.rs",
            vec![
                occ([0, 10, 13], ADD, role::IMPORT),
                occ([3, 12, 15], ADD, 0),
                occ([4, 19, 22], ADD, 0),
            ],
        ),
        doc(
            "src/util.rs",
            vec![
                occ([0, 7, 11], HELP, role::DEFINITION),
                occ([2, 1, 4], ADD, 0),
            ],
        ),
        doc("README.md", vec![]),
    ])
}

/// A repository using the package: its `main.rs` references `add` twice.
fn app_index() -> Vec<u8> {
    index(vec![doc(
        "src/main.rs",
        vec![occ([3, 12, 15], ADD, 0), occ([4, 12, 15], ADD, 0)],
    )])
}

fn graph_of(bytes: Vec<u8>) -> CodeGraph {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("graph");
    process_index(std::io::Cursor::new(bytes), "", &out, dir.path(), &|| false).unwrap();
    CodeGraph::open(&out).unwrap()
}

#[test]
fn exports_references_from_code_graphs() {
    let calc = export_upload(&graph_of(calc_index())).unwrap();
    let id = |p: &str| calc.paths.iter().position(|x| x == p).unwrap() as u32;
    let (lib, main, util) = (id("src/lib.rs"), id("src/main.rs"), id("src/util.rs"));
    let mut edges = calc.edges.clone();
    edges.sort_by_key(|e| (e.0, e.1));
    let mut want = vec![(main, lib, 3.0), (util, lib, 1.0), (lib, util, 1.0)];
    want.sort_by_key(|e| (e.0, e.1));
    // Local symbols and definitions give no edges; the import counts as a reference.
    assert_eq!(edges, want);
    assert!(calc.external.is_empty());
    let mut defs = vec![(symbol_hash(ADD), lib), (symbol_hash(HELP), util)];
    defs.sort_unstable();
    assert_eq!(calc.definitions, defs);

    // The app does not define `add`: its references go out by symbol hash.
    let app = export_upload(&graph_of(app_index())).unwrap();
    assert_eq!(app.paths, ["src/main.rs"]);
    assert!(app.edges.is_empty() && app.definitions.is_empty());
    assert_eq!(app.external, [(0, symbol_hash(ADD), 2)]);

    // Merged: the app's references reach calc's lib.rs, which ranks first.
    let uploads = vec![(RepoId(2), app), (RepoId(1), calc.clone())];
    let computed = compute_ranks(&uploads, &Options::default());
    assert!(computed.converged);
    assert_eq!(computed.paths, 5);
    assert_eq!(computed.edges, 4);
    let ranks: Vec<(RepoId, sourcegull_types::PathRanks)> = computed
        .ranks
        .iter()
        .map(|r| {
            (
                r.repo,
                sourcegull_types::PathRanks::decode(1, &r.payload).unwrap(),
            )
        })
        .collect();
    // The app's file and calc's README have nothing pointing at them: rank 0, so the app has
    // no ranks at all.
    assert_eq!(ranks.len(), 1);
    let (repo, calc_ranks) = &ranks[0];
    assert_eq!(*repo, RepoId(1));
    assert_eq!(calc_ranks.get("src/lib.rs"), 1.0);
    assert!(calc_ranks.get("src/util.rs") > 0.0 && calc_ranks.get("src/util.rs") < 1.0);
    assert_eq!(calc_ranks.get("src/main.rs"), 0.0);
    assert_eq!(calc_ranks.get("README.md"), 0.0);
    // The same inputs in another order give the same payloads.
    let reversed = vec![uploads[1].clone(), uploads[0].clone()];
    assert_eq!(compute_ranks(&reversed, &Options::default()), computed);
}

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Blobs,
    config: sourcegull_config::SiteConfigHandle,
    ranker: Ranker,
    cancel: CancellationToken,
    commit: String,
    ids: [RepoId; 2],
}

impl Drop for Env {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let src = SourceRepo::new();
    let commit = src.commit(
        "c1",
        &[
            ("src/lib.rs", "pub fn add() {}\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/util.rs", "pub fn help() {}\n"),
            ("README.md", "calc\n"),
        ],
    );
    let node = RepoNode::start(src.remote()).await;
    let mut ids = Vec::new();
    for (name, ext) in [(CALC, "1"), (APP, "2")] {
        let id: i32 = sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
             VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(name)
        .bind(ext)
        .fetch_one(db.pool())
        .await
        .unwrap();
        node.clone_repo(RepoId(id), name).await;
        ids.push(RepoId(id));
    }
    let dir = tempfile::tempdir().unwrap();
    let blobs: Blobs = Arc::new(FsBlobStore::open(dir.path().join("blobs")).unwrap());
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
    let config = ConfigHandle::new(1, SiteConfig::default());
    let graphs =
        Arc::new(GraphCache::new(&dir.path().join("graphs"), blobs.clone(), 1 << 30).unwrap());
    let ranker = Ranker {
        db: db.clone(),
        clients: node.clients.clone(),
        graphs,
        config: config.clone(),
        options: Options::default(),
    };
    Some(Env {
        _t: t,
        _src: src,
        _node: node,
        _dir: dir,
        db,
        blobs,
        config,
        ranker,
        cancel,
        commit,
        ids: [ids[0], ids[1]],
    })
}

impl Env {
    /// Uploads an index and waits until it is processed.
    async fn upload(&self, repo: RepoId, body: Vec<u8>) -> i64 {
        let store = CodeIntelStore::new(&self.db);
        let id = store
            .create_upload(&NewUpload {
                repo_id: repo,
                commit: self.commit.clone(),
                root: String::new(),
                indexer: "rust-analyzer".into(),
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
                UploadState::Completed => return id,
                UploadState::Queued | UploadState::Processing => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                s => panic!("upload {id} is {s:?}: {:?}", u.failure_message),
            }
        }
        panic!("upload {id} was not processed");
    }

    fn ranked(&self, outcome: PassOutcome) -> sourcegull_ranking::RankingReport {
        match outcome {
            PassOutcome::Ranked(r) => r,
            other => panic!("expected a run, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn ranking_passes() {
    let Some(env) = setup().await else {
        return;
    };
    let [calc, app] = env.ids;
    let store = RankingStore::new(&env.db);
    // Nothing to rank yet.
    assert_eq!(
        env.ranker.pass(false).await.unwrap(),
        PassOutcome::NoUploads
    );

    let calc_upload = env.upload(calc, calc_index()).await;
    env.upload(app, app_index()).await;
    let serving = env.ranker.serving_uploads().await.unwrap();
    assert_eq!(serving.len(), 2);
    let r = env.ranked(env.ranker.pass(false).await.unwrap());
    assert_eq!((r.uploads, r.paths, r.edges), (2, 5, 4));
    assert!(r.converged);
    assert_eq!(r.repos_changed, 1);
    assert_eq!(store.generation(calc).await.unwrap(), Some(r.run));
    assert_eq!(store.generation(app).await.unwrap(), None);
    let ranks = store.path_ranks(calc).await.unwrap().unwrap();
    assert_eq!(ranks.generation, r.run);
    assert_eq!(ranks.get("src/lib.rs"), 1.0);
    assert!(ranks.get("src/util.rs") > ranks.get("src/main.rs"));
    let totals = store.totals().await.unwrap();
    assert_eq!((totals.repos, totals.paths), (1, 2));

    // The same serving uploads: nothing to do.
    assert_eq!(
        env.ranker.pass(false).await.unwrap(),
        PassOutcome::Unchanged
    );
    // A requested rerun computes again; identical ranks keep their generation.
    store.request_rerun().await.unwrap();
    let again = env.ranked(env.ranker.pass(false).await.unwrap());
    assert!(again.run > r.run);
    assert_eq!(again.repos_changed, 0);
    assert_eq!(store.generation(calc).await.unwrap(), Some(r.run));
    let runs = store.runs(10).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].num_paths, Some(5));
    assert_eq!(
        runs[0].iterations,
        Some(i32::try_from(again.iterations).unwrap())
    );

    // Without calc's upload, nothing references anything: its ranks are dropped.
    CodeIntelStore::new(&env.db)
        .mark_deleting(&[calc_upload], "test")
        .await
        .unwrap();
    let dropped = env.ranked(env.ranker.pass(false).await.unwrap());
    assert_eq!(dropped.uploads, 1);
    assert_eq!(dropped.repos_changed, 1);
    assert_eq!(store.generation(calc).await.unwrap(), None);

    // Turned off: no passes.
    env.config.store(
        2,
        SiteConfig {
            codeintel_ranking_enabled: false,
            ..SiteConfig::default()
        },
    );
    assert_eq!(env.ranker.pass(true).await.unwrap(), PassOutcome::Disabled);
}
