//! Precise code navigation against the test Postgres, a repo node started
//! in-process over a temp git repo, and SCIP uploads processed by the real upload worker.
//!
//! Uploads are at the first commit and the file is viewed at the second, which inserts two
//! lines at the top and changes one, so positions go through adjustment.

use std::sync::Arc;
use std::time::Duration;

use async_graphql::{Request, Variables};
use prost::Message;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_blobstore::{Blobs, FsBlobStore};
use sourcegull_codeintel_scip::GraphCache;
use sourcegull_codeintel_scip::proto::{self, role};
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload, UploadState, upload_index_key};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_repos::Registry;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

const REPO: &str = "gitlab.example.com/acme/calc";
const OTHER: &str = "gitlab.example.com/acme/app";
const HIDDEN: &str = "gitlab.example.com/acme/secret";
/// Uses the package but has no upload.
const TOOL: &str = "gitlab.example.com/acme/tool";

const ADD: &str = "rust-analyzer cargo calc 1.0.0 add().";
/// `add` at other versions of the package.
const ADD_2: &str = "rust-analyzer cargo calc 2.0.0 add().";
const ADD_3: &str = "rust-analyzer cargo calc 3.0.0 add().";

const LIB: &str = "\
/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
";

const MAIN_1: &str = "\
use calc::add;

fn main() {
    let x = add(1, 2);
    println!(\"{}\", add(x, 3));
}
";

const MAIN_2: &str = "\
// Two new lines
// at the top.
use calc::add;

fn main() {
    let x = add(1, 2);
    println!(\"{}\", add(x, 30));
}
";

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
    let defined = occurrences
        .iter()
        .find(|o| o.symbol_roles == role::DEFINITION && !o.symbol.starts_with("local "));
    let symbols = if path == "src/lib.rs"
        && let Some(def) = defined
    {
        vec![proto::SymbolInformation {
            symbol: def.symbol.clone(),
            documentation: vec!["Adds two numbers.".into()],
            signature_documentation: Some(proto::Signature {
                language: "rust".into(),
                text: "pub fn add(a: i32, b: i32) -> i32".into(),
            }),
            ..Default::default()
        }]
    } else {
        Vec::new()
    };
    proto::Document {
        language: "rust".into(),
        relative_path: path.into(),
        position_encoding: proto::encoding::UTF8,
        occurrences,
        symbols,
    }
}

fn index(tool: &str, documents: Vec<proto::Document>) -> Vec<u8> {
    Index {
        metadata: Some(proto::Metadata {
            tool_info: Some(proto::ToolInfo {
                name: tool.into(),
                version: "1.0".into(),
                arguments: vec![],
            }),
            ..Default::default()
        }),
        documents,
    }
    .encode_to_vec()
}

/// The defining repository's index at the first commit.
fn calc_index() -> Vec<u8> {
    index(
        "rust-analyzer",
        vec![
            doc(
                "src/lib.rs",
                vec![
                    occ([1, 7, 10], ADD, role::DEFINITION),
                    occ([1, 11, 12], "local 0", role::DEFINITION),
                    occ([2, 4, 5], "local 0", 0),
                ],
            ),
            doc(
                "src/main.rs",
                vec![
                    occ([0, 10, 13], ADD, role::IMPORT),
                    occ([3, 8, 9], "local 1", role::DEFINITION),
                    occ([3, 12, 15], ADD, 0),
                    occ([4, 19, 22], ADD, 0),
                    occ([4, 23, 24], "local 1", 0),
                ],
            ),
        ],
    )
}

/// The defining repository's library alone, with `add` as `symbol`.
fn lib_index(symbol: &str) -> Vec<u8> {
    index(
        "rust-analyzer",
        vec![doc(
            "src/lib.rs",
            vec![occ([1, 7, 10], symbol, role::DEFINITION)],
        )],
    )
}

/// A repository that only uses the package.
fn app_index() -> Vec<u8> {
    index(
        "rust-analyzer",
        vec![doc("src/main.rs", vec![occ([3, 12, 15], ADD, 0)])],
    )
}

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    _dir: tempfile::TempDir,
    db: Db,
    blobs: Blobs,
    graphs: Arc<GraphCache>,
    cancel: CancellationToken,
    c1: String,
    c2: String,
    ids: [RepoId; 3],
    schema: SourcegullSchema,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, private, external_id, external_service_type, external_service_id) \
         VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .bind(private)
    .bind(ext)
    .fetch_one(db.pool())
    .await
    .unwrap();
    RepoId(id)
}

/// With `tool`, also [`TOOL`], indexed for search.
async fn setup(tool: bool) -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate");
    let db = Db::from_pool(tdb.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_public: true,
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let src = SourceRepo::new();
    let c1 = src.commit("c1", &[("src/lib.rs", LIB), ("src/main.rs", MAIN_1)]);
    let c2 = src.commit("c2", &[("src/main.rs", MAIN_2)]);
    let node = RepoNode::start(src.remote()).await;
    let mut ids = Vec::new();
    for (name, ext, private) in [(REPO, "1", false), (OTHER, "2", false), (HIDDEN, "3", true)] {
        let id = insert_repo(&db, name, ext, private).await;
        node.clone_repo(id, name).await;
        ids.push(id);
    }
    if tool {
        let id = insert_repo(&db, TOOL, "4", false).await;
        node.clone_repo(id, TOOL).await;
        node.index_repo(id, TOOL, &[]);
    }
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

    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle());
    let app = sourcegull_services::AppState::new(db.clone(), config.clone(), metrics, auth.clone())
        .with_repo_clients(node.clients.clone())
        .with_blobs(blobs.clone())
        .with_graph_cache(graphs.clone());
    let schema = build_schema(Services {
        db: db.clone(),
        auth,
        config,
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Registry::default(),
        repo_clients: node.clients.clone(),
        authz: app.authz.clone(),
        app: Some(app),
    });
    Some(Env {
        _t: tdb,
        _src: src,
        _node: node,
        _dir: dir,
        db,
        blobs,
        graphs,
        cancel,
        c1,
        c2,
        ids: [ids[0], ids[1], ids[2]],
        schema,
    })
}

impl Env {
    /// Uploads an index and waits until it is processed.
    async fn upload(&self, repo: RepoId, commit: &str, indexer: &str, body: Vec<u8>) -> i64 {
        let store = CodeIntelStore::new(&self.db);
        let id = store
            .create_upload(&NewUpload {
                repo_id: repo,
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

    async fn ok(&self, query: &str, vars: Value) -> Value {
        let req = Request::new(query).variables(Variables::from_json(vars));
        let resp = sourcegull_graphql::execute(&self.schema, req, None).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn symbol_info(&self, repo: &str, rev: &str, path: &str, line: i32, ch: i32) -> Value {
        let q = r#"query($repo: String!, $rev: String!, $path: String!, $line: Int!, $ch: Int!) {
            repository(name: $repo) { commit(rev: $rev) { blob(path: $path) {
                symbolInfo(line: $line, character: $ch) {
                    definition { repo commit path range { line character length } }
                    hover
                    range { start { line character } end { line character } }
                    provenance
                    crossVersion { requested found }
                }
            } } } }"#;
        let vars = json!({ "repo": repo, "rev": rev, "path": path, "line": line, "ch": ch });
        self.ok(q, vars).await["repository"]["commit"]["blob"]["symbolInfo"].clone()
    }

    async fn usages(&self, repo: &str, line: i32, start: i32, end: i32, extra: Value) -> Value {
        let q = r#"query($range: RangeInput!, $symbol: SymbolComparator, $filter: UsagesFilter,
                         $first: Int, $after: String) {
            usagesForSymbol(range: $range, symbol: $symbol, filter: $filter, first: $first,
                            after: $after) {
                nodes {
                    symbol { name }
                    provenance dataSource usageKind surroundingContent
                    usageRange { repository revision path range { start { line character } end { line character } } }
                    crossVersion { requested found }
                }
                pageInfo { hasNextPage endCursor }
                coverage {
                    preciseRepositories truncated summary
                    searchBasedRepositories { repository reason }
                    crossVersionRepositories { repository version }
                }
            } }"#;
        let mut vars = json!({ "range": {
            "repository": repo, "path": "src/main.rs",
            "start": { "line": line, "character": start }, "end": { "line": line, "character": end },
        } });
        if let Value::Object(m) = extra {
            vars.as_object_mut().unwrap().extend(m);
        }
        self.ok(q, vars).await["usagesForSymbol"].clone()
    }

    async fn precise_data(&self, rev: &str) -> Value {
        let q = r#"query($repo: String!, $rev: String!) {
            repository(name: $repo) { commit(rev: $rev) { blob(path: "src/main.rs") {
                codeGraphData(filter: { provenance: { equals: PRECISE } }) {
                    provenance commit toolInfo { name }
                    occurrences { nodes { symbol roles range { start { line character } end { line character } } } }
                }
            } } } }"#;
        self.ok(q, json!({ "repo": REPO, "rev": rev })).await["repository"]["commit"]["blob"]
            ["codeGraphData"]
            .clone()
    }
}

/// (repository, revision, path, line, character, kind) of each usage.
fn summary(conn: &Value) -> Vec<(String, String, String, i64, i64, String)> {
    conn["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            let r = &u["usageRange"];
            (
                r["repository"].as_str().unwrap().to_owned(),
                r["revision"].as_str().unwrap().to_owned(),
                r["path"].as_str().unwrap().to_owned(),
                r["range"]["start"]["line"].as_i64().unwrap(),
                r["range"]["start"]["character"].as_i64().unwrap(),
                u["usageKind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precise_navigation() {
    let Some(env) = setup(false).await else {
        return;
    };
    let [calc, app, hidden] = env.ids;
    env.upload(calc, &env.c1, "rust-analyzer", calc_index())
        .await;
    env.upload(app, &env.c1, "rust-analyzer", app_index()).await;
    // Newest, so it would come first if permissions were not applied.
    env.upload(hidden, &env.c1, "rust-analyzer", calc_index())
        .await;

    // Hover at HEAD over `add` in `let x = add(1, 2);` (line 3 at the upload's commit).
    let info = env.symbol_info(REPO, "main", "src/main.rs", 5, 13).await;
    assert_eq!(info["provenance"], "PRECISE", "{info}");
    assert_eq!(
        info["range"],
        json!({ "start": { "line": 5, "character": 12 }, "end": { "line": 5, "character": 15 } })
    );
    assert_eq!(
        info["definition"],
        json!({ "repo": REPO, "commit": env.c2, "path": "src/lib.rs",
                "range": { "line": 1, "character": 7, "length": 3 } })
    );
    assert_eq!(
        info["hover"],
        "```rust\npub fn add(a: i32, b: i32) -> i32\n```\n\n---\n\nAdds two numbers."
    );
    // The changed line has no precise data: search-based navigation answers.
    let info = env.symbol_info(REPO, "main", "src/main.rs", 6, 20).await;
    assert_ne!(info["provenance"], "PRECISE", "{info}");

    // Usages: the file first, then its repository, then other repositories at their
    // uploads' commit; the changed line is dropped and the private repository never shows.
    let t = std::time::Instant::now();
    let all = env.usages(REPO, 5, 12, 15, json!({})).await;
    eprintln!("usages, precise only: {:?}", t.elapsed());
    let (c1, c2) = (env.c1.as_str(), env.c2.as_str());
    let u = |repo: &str, rev: &str, path: &str, line, ch, kind: &str| {
        (
            repo.to_owned(),
            rev.to_owned(),
            path.to_owned(),
            line,
            ch,
            kind.to_owned(),
        )
    };
    let expected = vec![
        u(REPO, c2, "src/main.rs", 2, 10, "REFERENCE"),
        u(REPO, c2, "src/main.rs", 5, 12, "REFERENCE"),
        u(REPO, c2, "src/lib.rs", 1, 7, "DEFINITION"),
        u(OTHER, c1, "src/main.rs", 3, 12, "REFERENCE"),
    ];
    assert_eq!(summary(&all), expected, "{all}");
    let n = &all["nodes"][1];
    assert_eq!(n["provenance"], "PRECISE");
    assert_eq!(n["dataSource"], "scip");
    assert_eq!(n["symbol"]["name"], ADD);
    assert_eq!(n["surroundingContent"], "    let x = add(1, 2);");

    // Pages cover the same list.
    let page = env.usages(REPO, 5, 12, 15, json!({ "first": 3 })).await;
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let rest = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "first": 3, "after": page["pageInfo"]["endCursor"] }),
        )
        .await;
    assert_eq!(rest["pageInfo"]["hasNextPage"], false);
    let mut paged = summary(&page);
    paged.extend(summary(&rest));
    assert_eq!(paged, expected);

    // Repository filters and provenance.
    let only_other = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "filter": { "repository": { "name": { "equals": OTHER } } } }),
        )
        .await;
    assert_eq!(summary(&only_other), expected[3..].to_vec());
    let not_other = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "filter": { "not": { "repository": { "name": { "equals": OTHER } } } } }),
        )
        .await;
    assert_eq!(summary(&not_other), expected[..3].to_vec());
    let syntactic = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "symbol": { "name": { "equals": ADD }, "provenance": { "equals": "SYNTACTIC" } } }),
        )
        .await;
    assert_eq!(syntactic["nodes"], json!([]));

    // A local symbol stays in its file; its reference on the changed line is dropped.
    let local = env.usages(REPO, 5, 8, 9, json!({})).await;
    assert_eq!(
        summary(&local),
        vec![u(REPO, c2, "src/main.rs", 5, 8, "DEFINITION")]
    );
    assert_eq!(local["nodes"][0]["symbol"]["name"], "local 1");
    assert_eq!(local["coverage"], Value::Null);
    assert_eq!(
        all["coverage"],
        json!({ "preciseRepositories": [REPO, OTHER], "crossVersionRepositories": [],
                "searchBasedRepositories": [],
                "truncated": false,
                "summary": "precise: 2 repositories · search-based: none" })
    );

    // From the repository that only uses the package, the definition comes through the
    // package moniker (at the defining upload's commit), the hover from its upload.
    let info = env.symbol_info(OTHER, "main", "src/main.rs", 5, 13).await;
    assert_eq!(info["provenance"], "PRECISE", "{info}");
    assert_eq!(
        info["definition"],
        json!({ "repo": REPO, "commit": env.c1, "path": "src/lib.rs",
                "range": { "line": 1, "character": 7, "length": 3 } })
    );
    assert!(
        info["hover"]
            .as_str()
            .unwrap()
            .ends_with("Adds two numbers.")
    );

    // Code graph data at the viewed commit.
    let data = env.precise_data("main").await;
    let d = &data[0];
    assert_eq!(data.as_array().unwrap().len(), 1, "{data}");
    assert_eq!(d["provenance"], "PRECISE");
    assert_eq!(d["commit"], env.c1.as_str());
    assert_eq!(d["toolInfo"]["name"], "rust-analyzer");
    let found: Vec<(String, i64, i64)> = d["occurrences"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["symbol"].as_str().unwrap().to_owned(),
                o["range"]["start"]["line"].as_i64().unwrap(),
                o["range"]["start"]["character"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (ADD.to_owned(), 2, 10),
            ("local 1".to_owned(), 5, 8),
            (ADD.to_owned(), 5, 12)
        ]
    );

    // Each upload's graph was fetched once.
    assert_eq!(env.graphs.downloads(), 2);

    // A descendant's upload serves an older commit when no ancestor has one for its indexer.
    let extra = index(
        "scip-extra",
        vec![doc("src/main.rs", vec![occ([5, 12, 15], ADD, 0)])],
    );
    env.upload(calc, &env.c2, "scip-extra", extra).await;
    let data = env.precise_data(&env.c1).await;
    let tools: Vec<(&str, &str, &Value)> = data
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["toolInfo"]["name"].as_str().unwrap(),
                d["commit"].as_str().unwrap(),
                &d["occurrences"]["nodes"][0]["range"]["start"],
            )
        })
        .collect();
    assert_eq!(
        tools,
        [
            ("rust-analyzer", c1, &json!({ "line": 0, "character": 10 })),
            ("scip-extra", c2, &json!({ "line": 3, "character": 12 })),
        ]
    );

    // isLatestForRepo: the uploads serving the default branch head.
    let latest = || async {
        let q = r#"{ preciseIndexes(first: 50) { nodes {
            inputCommit inputIndexer isLatestForRepo projectRoot { repository { name } } } } }"#;
        let data = env.ok(q, json!({})).await;
        let mut rows: Vec<(String, String, bool)> = data["preciseIndexes"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["projectRoot"]["repository"]["name"] == REPO)
            .map(|n| {
                (
                    n["inputIndexer"].as_str().unwrap().to_owned(),
                    n["inputCommit"].as_str().unwrap().to_owned(),
                    n["isLatestForRepo"].as_bool().unwrap(),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    let s = |a: &str, b: &str, l| (a.to_owned(), b.to_owned(), l);
    assert_eq!(
        latest().await,
        [s("rust-analyzer", c1, true), s("scip-extra", c2, true)]
    );
    env.upload(calc, &env.c2, "rust-analyzer", calc_index())
        .await;
    let mut expected = vec![
        s("rust-analyzer", c1, false),
        s("rust-analyzer", c2, true),
        s("scip-extra", c2, true),
    ];
    expected.sort();
    assert_eq!(latest().await, expected);
}

/// A repository that uses the package without an upload answers search-based, after the
/// precise usages. (Requests use a filter of their own, so the usage caches, shared by the
/// tests of this binary, keep them apart from [`precise_navigation`].)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precise_usages_complemented() {
    let Some(env) = setup(true).await else { return };
    let [calc, app, _] = env.ids;
    env.upload(calc, &env.c1, "rust-analyzer", calc_index())
        .await;
    env.upload(app, &env.c1, "rust-analyzer", app_index()).await;
    let (c1, c2) = (env.c1.as_str(), env.c2.as_str());
    let u = |repo: &str, rev: &str, path: &str, line, ch, kind: &str| {
        (
            repo.to_owned(),
            rev.to_owned(),
            path.to_owned(),
            line,
            ch,
            kind.to_owned(),
        )
    };
    let expected = vec![
        u(REPO, c2, "src/main.rs", 2, 10, "REFERENCE"),
        u(REPO, c2, "src/main.rs", 5, 12, "REFERENCE"),
        u(REPO, c2, "src/lib.rs", 1, 7, "DEFINITION"),
        u(OTHER, c1, "src/main.rs", 3, 12, "REFERENCE"),
    ];
    let not_hidden =
        json!({ "filter": { "not": { "repository": { "name": { "equals": HIDDEN } } } } });
    let t = std::time::Instant::now();
    let more = env.usages(REPO, 5, 12, 15, not_hidden.clone()).await;
    let with_additions = t.elapsed();
    eprintln!("usages with search-based additions: {with_additions:?}");
    let got = summary(&more);
    assert_eq!(got[..4], expected[..], "{more}");
    let added = &got[4..];
    assert!(
        added.contains(&u(TOOL, c2, "src/main.rs", 5, 12, "REFERENCE")),
        "{more}"
    );
    assert!(added.iter().all(|a| a.0 == TOOL), "{more}");
    let nodes = more["nodes"].as_array().unwrap();
    assert!(nodes[..4].iter().all(|n| n["provenance"] == "PRECISE"));
    assert!(nodes[4..].iter().all(|n| n["provenance"] == "SEARCH_BASED"));
    assert_eq!(
        more["coverage"],
        json!({ "preciseRepositories": [REPO, OTHER], "crossVersionRepositories": [],
                "searchBasedRepositories": [{ "repository": TOOL, "reason": "NO_INDEX" }],
                "truncated": false,
                "summary": format!("precise: 2 repositories · search-based: {TOOL} (no index)") })
    );
    // Pages: precise first, then the additions.
    let page = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "first": 3, "filter": not_hidden["filter"] }),
        )
        .await;
    let rest = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "first": 100, "after": page["pageInfo"]["endCursor"], "filter": not_hidden["filter"] }),
        )
        .await;
    let mut paged = summary(&page);
    paged.extend(summary(&rest));
    assert_eq!(paged, got);
    // Asking for precise usages leaves the additions out.
    let precise = env
        .usages(
            REPO,
            5,
            12,
            15,
            json!({ "filter": not_hidden["filter"],
                    "symbol": { "name": { "equals": ADD }, "provenance": { "equals": "PRECISE" } } }),
        )
        .await;
    assert_eq!(summary(&precise), expected);
}

/// A library indexed at one version links to users of another: go to definition, and
/// references in both directions, marked as linked across versions. Exact matches win.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precise_cross_version() {
    let Some(env) = setup(false).await else {
        return;
    };
    let [calc, app, _] = env.ids;
    env.upload(calc, &env.c1, "rust-analyzer", lib_index(ADD_2))
        .await;
    env.upload(app, &env.c1, "rust-analyzer", app_index()).await;
    let (c1, c2) = (env.c1.as_str(), env.c2.as_str());
    let cross = |requested: &str, found: &str| json!({ "requested": requested, "found": found });

    // The user's `add` is at 1.0.0; only 2.0.0 is indexed.
    let info = env.symbol_info(OTHER, "main", "src/main.rs", 5, 13).await;
    assert_eq!(info["provenance"], "PRECISE", "{info}");
    assert_eq!(
        info["definition"],
        json!({ "repo": REPO, "commit": c1, "path": "src/lib.rs",
                "range": { "line": 1, "character": 7, "length": 3 } })
    );
    assert_eq!(info["crossVersion"], cross("1.0.0", "2.0.0"));
    assert!(
        info["hover"]
            .as_str()
            .unwrap()
            .ends_with("Adds two numbers."),
        "{info}"
    );

    // A filter of this test's own keeps the usage caches apart from the other tests'.
    let filter = json!({ "not": { "repository": { "name": { "equals": HIDDEN } } } });
    let u = |repo: &str, rev: &str, path: &str, line, ch, kind: &str| {
        (
            repo.to_owned(),
            rev.to_owned(),
            path.to_owned(),
            line,
            ch,
            kind.to_owned(),
        )
    };

    // References from the library include the user at the other version.
    let range = json!({ "repository": REPO, "path": "src/lib.rs",
        "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 10 } });
    let from_lib = env
        .usages(REPO, 0, 0, 0, json!({ "range": range, "filter": filter }))
        .await;
    assert_eq!(
        summary(&from_lib),
        vec![
            u(REPO, c2, "src/lib.rs", 1, 7, "DEFINITION"),
            u(OTHER, c1, "src/main.rs", 3, 12, "REFERENCE"),
        ],
        "{from_lib}"
    );
    assert_eq!(from_lib["nodes"][0]["crossVersion"], Value::Null);
    assert_eq!(
        from_lib["nodes"][1]["crossVersion"],
        cross("2.0.0", "1.0.0")
    );
    assert_eq!(from_lib["nodes"][1]["symbol"]["name"], ADD);
    assert_eq!(
        from_lib["coverage"],
        json!({ "preciseRepositories": [REPO, OTHER],
                "crossVersionRepositories": [{ "repository": OTHER, "version": "1.0.0" }],
                "searchBasedRepositories": [], "truncated": false,
                "summary": format!("precise: 2 repositories · other versions: {OTHER} (1.0.0) · search-based: none") })
    );

    // References from the user include the library's definition.
    let from_app = env
        .usages(OTHER, 5, 12, 15, json!({ "filter": filter }))
        .await;
    assert_eq!(
        summary(&from_app),
        vec![
            u(OTHER, c2, "src/main.rs", 5, 12, "REFERENCE"),
            u(REPO, c1, "src/lib.rs", 1, 7, "DEFINITION"),
        ],
        "{from_app}"
    );
    assert_eq!(
        from_app["nodes"][1]["crossVersion"],
        cross("1.0.0", "2.0.0")
    );

    // The upload serving the library's default branch wins over a newer one.
    env.upload(calc, &env.c2, "rust-analyzer", lib_index(ADD_2))
        .await;
    env.upload(calc, &env.c1, "rust-analyzer", lib_index(ADD_3))
        .await;
    let info = env.symbol_info(OTHER, "main", "src/main.rs", 5, 13).await;
    assert_eq!(info["definition"]["commit"], c2, "{info}");
    assert_eq!(info["crossVersion"], cross("1.0.0", "2.0.0"));

    // An exact version wins.
    env.upload(calc, &env.c1, "rust-analyzer", lib_index(ADD))
        .await;
    let info = env.symbol_info(OTHER, "main", "src/main.rs", 5, 13).await;
    assert_eq!(info["definition"]["commit"], c1, "{info}");
    assert_eq!(info["crossVersion"], Value::Null, "{info}");
}
