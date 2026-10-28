//! Code browsing GraphQL (commits, trees, blobs, highlighting, blame, refs, comparisons)
//! against the test Postgres and a real repo node started in-process over a temp git repo.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_repos::Registry;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{BrowseRepoOids, RepoNode, SourceRepo, browse_repo};
use sourcegull_types::RepoId;

const REPO: &str = "gitlab.example.com/acme/demo";
const UNCLONED: &str = "gitlab.example.com/acme/uncloned";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    oids: BrowseRepoOids,
    schema: SourcegullSchema,
}

async fn insert_repo(db: &Db, name: &str, ext: &str) -> RepoId {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ($1, $2, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .bind(ext)
    .fetch_one(db.pool())
    .await
    .unwrap();
    RepoId(id)
}

async fn setup() -> Option<Env> {
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
    let (src, oids) = browse_repo();
    let node = RepoNode::start(src.remote()).await;
    let id = insert_repo(&db, REPO, "1").await;
    insert_repo(&db, UNCLONED, "2").await;
    node.clone_repo(id, REPO).await;
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle());
    let app = sourcegull_services::AppState::new(db.clone(), config.clone(), metrics, auth.clone())
        .with_repo_clients(node.clients.clone());
    let schema = build_schema(Services {
        db,
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
        oids,
        schema,
    })
}

impl Env {
    async fn run(&self, query: &str, vars: Value) -> async_graphql::Response {
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, None).await
    }

    async fn ok(&self, query: &str, vars: Value) -> Value {
        let resp = self.run(query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }
}

#[tokio::test]
async fn commits_trees_and_blobs() {
    let Some(env) = setup().await else { return };
    let o = &env.oids;
    let data = env
        .ok(
            r#"query($name: String!) { repository(name: $name) {
                commit(rev: "main~1") {
                    oid abbreviatedOID subject body message url canonicalURL
                    author { person { name email displayName } date }
                    committer { person { name } }
                    parents { oid subject }
                    tree { isRoot url entries { __typename name path isDirectory kind url } }
                    src: tree(path: "src") { path name url entries { name } }
                    notADir: tree(path: "README.md") { path }
                    missing: blob(path: "nope.txt") { path }
                    dirAsBlob: blob(path: "src") { path }
                    blob(path: "src/main.rs") {
                        name path url canonicalURL rawURL byteSize binary totalLines languages
                        contentTruncated content
                        lines: content(startLine: 2, endLine: 3)
                        highlight { aborted packed version language engine plainReason }
                        commit { oid }
                    }
                    img: file(path: "img.png") { binary content byteSize }
                    path(path: "docs") { __typename ... on GitTree { isDirectory } }
                }
                nope: commit(rev: "does-not-exist") { oid }
            } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let c = &data["repository"]["commit"];
    assert_eq!(c["oid"], o.c2.as_str());
    assert_eq!(c["abbreviatedOID"], &o.c2[..7]);
    assert_eq!(c["subject"], "c2: greet");
    assert_eq!(c["body"], "Longer body.");
    assert_eq!(c["url"], format!("/{REPO}/-/commit/{}", o.c2));
    assert_eq!(c["author"]["person"]["name"], "Ann Author");
    assert_eq!(c["author"]["person"]["email"], "ann@example.com");
    assert!(
        c["author"]["date"]
            .as_str()
            .unwrap()
            .starts_with("2023-11-14T")
    );
    assert_eq!(c["committer"]["person"]["name"], "Cid Committer");
    assert_eq!(
        c["parents"],
        json!([{ "oid": o.c1, "subject": "c1: initial" }])
    );

    let tree = &c["tree"];
    assert_eq!(tree["isRoot"], true);
    assert_eq!(tree["url"], format!("/{REPO}@main~1/-/tree"));
    let names: Vec<&str> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    // Directories first, then files, each by name.
    assert_eq!(names, ["docs", "src", "README.md", "img.png"]);
    assert_eq!(tree["entries"][0]["__typename"], "GitTree");
    assert_eq!(tree["entries"][0]["kind"], "DIRECTORY");
    assert_eq!(tree["entries"][2]["__typename"], "GitBlob");
    assert_eq!(
        tree["entries"][2]["url"],
        format!("/{REPO}@main~1/-/blob/README.md")
    );
    assert_eq!(c["src"]["name"], "src");
    assert_eq!(
        c["src"]["entries"],
        json!([{ "name": "lib.rs" }, { "name": "main.rs" }])
    );
    assert_eq!(c["notADir"], Value::Null);
    assert_eq!(c["missing"], Value::Null);
    assert_eq!(c["dirAsBlob"], Value::Null);

    let b = &c["blob"];
    let content = "fn main() {\n    println!(\"hello, world\");\n    run();\n}\n";
    assert_eq!(b["content"], content);
    assert_eq!(b["lines"], "    println!(\"hello, world\");\n    run();\n");
    assert_eq!(b["byteSize"], content.len());
    assert_eq!(b["binary"], false);
    assert_eq!(b["contentTruncated"], false);
    assert_eq!(b["totalLines"], 4);
    assert_eq!(b["languages"], json!(["Rust"]));
    assert_eq!(
        b["canonicalURL"],
        format!("/{REPO}@{}/-/blob/src/main.rs", o.c2)
    );
    assert_eq!(b["rawURL"], format!("/{REPO}@{}/-/raw/src/main.rs", o.c2));
    assert_eq!(b["commit"]["oid"], o.c2.as_str());
    let hl = &b["highlight"];
    assert_eq!(hl["aborted"], false);
    assert_eq!(hl["engine"], "treeSitter");
    assert_eq!(hl["language"], "rust");
    assert_eq!(hl["version"], sourcegull_highlight::VERSION);
    let packed: Vec<u32> = hl["packed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    let occ = sourcegull_highlight::unpack(&packed).unwrap();
    // `fn` at the start of line 0 is a keyword.
    assert!(
        occ.iter().any(|o| o.line == 0
            && o.start_col == 0
            && o.end_col == 2
            && o.kind == sourcegull_highlight::SyntaxKind::Keyword),
        "{occ:?}"
    );

    assert_eq!(c["img"]["binary"], true);
    assert_eq!(c["img"]["content"], "");
    assert_eq!(c["img"]["byteSize"], 8);
    assert_eq!(
        c["path"],
        json!({ "__typename": "GitTree", "isDirectory": true })
    );
    assert_eq!(data["repository"]["nope"], Value::Null);

    // The highlight cache answers the second request identically.
    let again = env
        .ok(
            r#"query($name: String!) { repository(name: $name) { commit(rev: "main~1") {
                blob(path: "src/main.rs") { highlight { packed } } } } }"#,
            json!({ "name": REPO }),
        )
        .await;
    assert_eq!(
        again["repository"]["commit"]["blob"]["highlight"]["packed"],
        hl["packed"]
    );
}

#[tokio::test]
async fn history_refs_and_blame() {
    let Some(env) = setup().await else { return };
    let o = &env.oids;
    let q = r#"query($name: String!, $after: String) { repository(name: $name) {
        commit(rev: "main") {
            ancestors(first: 2, afterCursor: $after) {
                nodes { oid subject } pageInfo { hasNextPage endCursor }
            }
            mainHistory: ancestors(path: "src/main.rs") { nodes { subject } }
        }
    } }"#;
    let page1 = env.ok(q, json!({ "name": REPO })).await;
    let a = &page1["repository"]["commit"]["ancestors"];
    assert_eq!(
        a["nodes"],
        json!([{ "oid": o.c3, "subject": "c3: docs" }, { "oid": o.c2, "subject": "c2: greet" }])
    );
    assert_eq!(a["pageInfo"]["hasNextPage"], true);
    let page2 = env
        .ok(
            q,
            json!({ "name": REPO, "after": a["pageInfo"]["endCursor"] }),
        )
        .await;
    let a2 = &page2["repository"]["commit"]["ancestors"];
    assert_eq!(
        a2["nodes"],
        json!([{ "oid": o.c1, "subject": "c1: initial" }])
    );
    assert_eq!(a2["pageInfo"]["hasNextPage"], false);
    assert_eq!(
        page1["repository"]["commit"]["mainHistory"]["nodes"],
        json!([{ "subject": "c2: greet" }, { "subject": "c1: initial" }])
    );

    let refs = env
        .ok(
            r#"query($name: String!) { repository(name: $name) {
                defaultBranch { name displayName abbrevName type prefix url }
                branches { totalCount nodes { displayName target { oid commit { subject } } } }
                alpha: branches(orderBy: ALPHABETICAL, first: 1) {
                    totalCount nodes { displayName } pageInfo { hasNextPage }
                }
                tags { nodes { name displayName type annotated target { oid } } }
                filtered: gitRefs(query: "fea") { nodes { displayName type } }
            } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let r = &refs["repository"];
    assert_eq!(
        r["defaultBranch"],
        json!({
            "name": "refs/heads/main", "displayName": "main", "abbrevName": "main",
            "type": "GIT_BRANCH", "prefix": "refs/heads/", "url": format!("/{REPO}@main")
        })
    );
    // Most recently committed first: main (c3) was committed after feature (f1).
    assert_eq!(r["branches"]["totalCount"], 2);
    assert_eq!(r["branches"]["nodes"][0]["displayName"], "main");
    assert_eq!(r["branches"]["nodes"][0]["target"]["oid"], o.c3.as_str());
    assert_eq!(
        r["branches"]["nodes"][1]["target"]["commit"]["subject"],
        "f1: subtract"
    );
    assert_eq!(r["alpha"]["nodes"], json!([{ "displayName": "feature" }]));
    assert_eq!(r["alpha"]["pageInfo"]["hasNextPage"], true);
    let tags = r["tags"]["nodes"].as_array().unwrap();
    assert_eq!(tags.len(), 2);
    let v1 = tags.iter().find(|t| t["displayName"] == "v1.0").unwrap();
    assert_eq!(v1["annotated"], true);
    assert_eq!(v1["target"]["oid"], o.c3.as_str());
    assert_eq!(v1["type"], "GIT_TAG");
    assert_eq!(
        r["filtered"]["nodes"],
        json!([{ "displayName": "feature", "type": "GIT_BRANCH" }])
    );

    let blame = env
        .ok(
            r#"query($name: String!) { repository(name: $name) { commit(rev: "main") {
                blob(path: "src/main.rs") {
                    blame { startLine endLine rev message filename author { person { name } }
                            commit { oid parents { oid } } }
                    part: blame(startLine: 2, endLine: 2) { startLine endLine rev }
                } } } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let b = &blame["repository"]["commit"]["blob"];
    let hunks = b["blame"].as_array().unwrap();
    // Lines 1 and 4 are from c1, lines 2-3 from c2.
    let spans: Vec<(i64, i64, &str)> = hunks
        .iter()
        .map(|h| {
            (
                h["startLine"].as_i64().unwrap(),
                h["endLine"].as_i64().unwrap(),
                h["rev"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        spans,
        [
            (1, 2, o.c1.as_str()),
            (2, 4, o.c2.as_str()),
            (4, 5, o.c1.as_str())
        ]
    );
    assert_eq!(hunks[1]["message"], "c2: greet");
    assert_eq!(hunks[1]["filename"], "src/main.rs");
    assert_eq!(hunks[1]["author"]["person"]["name"], "Ann Author");
    assert_eq!(hunks[1]["commit"]["parents"], json!([{ "oid": o.c1 }]));
    // A range clips the hunk to the requested lines.
    assert_eq!(
        b["part"],
        json!([{ "startLine": 2, "endLine": 3, "rev": o.c2 }])
    );
}

#[tokio::test]
async fn comparisons_and_commit_diffs() {
    let Some(env) = setup().await else { return };
    let o = &env.oids;
    let data = env
        .ok(
            r#"query($name: String!) { repository(name: $name) {
                comparison(base: "main", head: "feature") {
                    range { expr baseRevSpec { expr object { oid } } headRevSpec { object { oid } }
                            mergeBase { oid } }
                    commits { nodes { subject } }
                    fileDiffs {
                        totalCount diffStat { added deleted }
                        nodes { oldPath newPath status stat { added deleted }
                                oldFile { content } newFile { path }
                                hunks { oldRange { startLine lines } newRange { startLine lines } body } }
                    }
                }
                commit(rev: "main~1") {
                    diff { range { expr } commits { nodes { oid } }
                           fileDiffs { nodes { newPath oldFile { commit { oid } } hunks { body } } } }
                }
                root: commit(rev: "light") { diff { fileDiffs { totalCount nodes { status oldFile { path } } } } }
            } }"#,
            json!({ "name": REPO }),
        )
        .await;
    let cmp = &data["repository"]["comparison"];
    assert_eq!(cmp["range"]["expr"], "main...feature");
    assert_eq!(cmp["range"]["baseRevSpec"]["expr"], "main");
    assert_eq!(cmp["range"]["baseRevSpec"]["object"]["oid"], o.c3.as_str());
    assert_eq!(cmp["range"]["headRevSpec"]["object"]["oid"], o.f1.as_str());
    assert_eq!(cmp["range"]["mergeBase"]["oid"], o.c2.as_str());
    assert_eq!(
        cmp["commits"]["nodes"],
        json!([{ "subject": "f1: subtract" }])
    );
    let fd = &cmp["fileDiffs"];
    assert_eq!(fd["totalCount"], 1);
    assert_eq!(fd["diffStat"], json!({ "added": 4, "deleted": 0 }));
    let f = &fd["nodes"][0];
    assert_eq!(f["oldPath"], "src/lib.rs");
    assert_eq!(f["newPath"], "src/lib.rs");
    assert_eq!(f["status"], "MODIFIED");
    assert!(
        f["oldFile"]["content"]
            .as_str()
            .unwrap()
            .starts_with("pub fn add")
    );
    assert_eq!(f["hunks"][0]["newRange"]["startLine"], 1);
    assert!(
        f["hunks"][0]["body"]
            .as_str()
            .unwrap()
            .contains("+pub fn sub")
    );

    let d = &data["repository"]["commit"]["diff"];
    assert_eq!(d["range"]["expr"], format!("{}..main~1", o.c1));
    assert_eq!(d["commits"]["nodes"], json!([{ "oid": o.c2 }]));
    assert_eq!(d["fileDiffs"]["nodes"][0]["newPath"], "src/main.rs");
    assert_eq!(
        d["fileDiffs"]["nodes"][0]["oldFile"]["commit"]["oid"],
        o.c1.as_str()
    );
    assert!(
        d["fileDiffs"]["nodes"][0]["hunks"][0]["body"]
            .as_str()
            .unwrap()
            .contains("+    run();")
    );

    let root = &data["repository"]["root"]["diff"]["fileDiffs"];
    assert_eq!(root["totalCount"], 5);
    assert!(
        root["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["status"] == "ADDED" && n["oldFile"].is_null())
    );
}

#[tokio::test]
async fn uncloned_repositories_report_a_code() {
    let Some(env) = setup().await else { return };
    let resp = env
        .run(
            r#"query($name: String!) { repository(name: $name) {
                defaultBranch { name } commit(rev: "HEAD") { oid } } }"#,
            json!({ "name": UNCLONED }),
        )
        .await;
    assert_eq!(resp.errors.len(), 1, "{:?}", resp.errors);
    let code = resp.errors[0]
        .extensions
        .as_ref()
        .and_then(|e| e.get("code"))
        .cloned();
    assert_eq!(code, Some(async_graphql::Value::from("REPO_NOT_CLONED")));
    // defaultBranch degrades to null instead of failing.
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["repository"]["defaultBranch"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_aggregations() {
    let Some(env) = setup().await else { return };
    let q = r#"query($q: String!, $t: SearchPatternType!, $mode: SearchAggregationMode) {
        searchQueryAggregate(query: $q, patternType: $t) {
            modeAvailability { mode available reasonUnavailable }
            aggregations(mode: $mode, limit: 1) {
                __typename
                ... on ExhaustiveSearchAggregationResult {
                    mode groups { label count query } otherResultCount otherGroupCount
                }
                ... on SearchAggregationNotAvailable { reason reasonType mode }
            }
        } }"#;
    let agg = |query: &str, t: &str, mode: Value| {
        let vars = json!({ "q": query, "t": t, "mode": mode });
        let env = &env;
        async move { env.ok(q, vars).await["searchQueryAggregate"].clone() }
    };

    // Paths, the largest group first.
    let a = agg("repo:demo$ hello", "keyword", json!("PATH")).await;
    let modes: Vec<(&str, bool)> = a["modeAvailability"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["mode"].as_str().unwrap(),
                m["available"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        modes,
        [
            ("REPO", true),
            ("PATH", true),
            ("AUTHOR", false),
            ("CAPTURE_GROUP", false)
        ]
    );
    let r = &a["aggregations"];
    assert_eq!(r["__typename"], "ExhaustiveSearchAggregationResult", "{r}");
    assert_eq!(r["groups"].as_array().unwrap().len(), 1);
    assert_eq!(r["otherGroupCount"], 1);
    let first = &r["groups"][0];
    assert!(
        first["query"]
            .as_str()
            .unwrap()
            .starts_with("repo:demo$ hello file:^")
    );

    // A capture group is the default for a regexp with one.
    let a = agg(r"repo:demo$ fn\s(\w+)", "regexp", Value::Null).await;
    let r = &a["aggregations"];
    assert_eq!(r["mode"], "CAPTURE_GROUP", "{r}");
    // Every function has one definition: ties come in label order.
    let label = r["groups"][0]["label"].as_str().unwrap();
    assert_eq!(label, "add");
    assert_eq!(r["groups"][0]["query"], format!(r"repo:demo$ fn\s{label}"));

    // Authors of commits.
    let a = agg("repo:demo$ type:commit greet", "keyword", Value::Null).await;
    let r = &a["aggregations"];
    assert_eq!(r["mode"], "AUTHOR", "{r}");
    assert_eq!(r["groups"][0]["label"], "Ann Author");

    // Modes a query does not support, and invalid queries.
    let a = agg("repo:demo$ hello", "keyword", json!("AUTHOR")).await;
    assert_eq!(
        a["aggregations"]["reasonType"],
        "INVALID_AGGREGATION_MODE_FOR_QUERY"
    );
    // A lone `(` is pattern text in keyword queries; an unknown type is not.
    let a = agg("type:bogus hello", "keyword", Value::Null).await;
    assert_eq!(a["aggregations"]["reasonType"], "INVALID_QUERY");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compute_results() {
    let Some(env) = setup().await else { return };
    let q = r#"query($q: String!) {
        compute(query: $q) {
            __typename
            ... on ComputeMatchContext {
                repository { name } commit path
                matches { value range { start { line character } }
                          environment { variable value range { start { line character } } } }
            }
            ... on ComputeText { repository { name } commit path kind value }
        } }"#;
    let compute = |query: &str| {
        let vars = json!({ "q": query });
        let env = &env;
        async move { env.ok(q, vars).await["compute"].clone() }
    };

    let r = compute(r#"repo:demo$ println!\("(?P<word>\w+)"#).await;
    let r = r.as_array().unwrap();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["__typename"], "ComputeMatchContext");
    assert_eq!(r[0]["repository"]["name"], "gitlab.example.com/acme/demo");
    assert_eq!(r[0]["commit"], env.oids.c3);
    assert_eq!(r[0]["path"], "src/main.rs");
    let entry = &r[0]["matches"][0]["environment"][0];
    assert_eq!(entry["variable"], "word");
    assert_eq!(entry["value"], "hello");
    assert_eq!(
        entry["range"]["start"],
        json!({ "line": 1, "character": 14 })
    );

    let r = compute(r"repo:demo$ file:lib content:output(fn (\w+) -> $1:$repo)").await;
    assert_eq!(r[0]["__typename"], "ComputeText");
    assert_eq!(r[0]["kind"], "output");
    assert_eq!(r[0]["path"], "src/lib.rs");
    assert_eq!(r[0]["value"], "add:gitlab.example.com/acme/demo\n");

    // Invalid compute queries are errors.
    let resp = env.run(q, json!({ "q": "content:output(nope)" })).await;
    assert_eq!(resp.errors.len(), 1);
    assert!(resp.errors[0].message.contains("->"), "{:?}", resp.errors);
}
