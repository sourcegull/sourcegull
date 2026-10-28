//! Code navigation GraphQL: search-based `symbolInfo` and `usagesForSymbol` against
//! the test Postgres and a real repo node started in-process over a temp git repo.

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
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::RepoId;

const REPO: &str = "gitlab.example.com/acme/calc";
const OTHER: &str = "gitlab.example.com/acme/calc-fork";
const HIDDEN: &str = "gitlab.example.com/acme/secret";

const LIB: &str = "\
/// Adds two numbers.
///
/// Wraps on overflow.
#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
";

const MAIN: &str = "\
use calc::add;

fn main() {
    // add is not called in this comment
    let label = \"add\";
    println!(\"{} {}\", label, add(1, 2));
    let naïve = add(3, 4);
}
";

struct Env {
    _t: TestDb,
    _src: SourceRepo,
    _node: RepoNode,
    commit: String,
    schema: SourcegullSchema,
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
    let src = SourceRepo::new();
    let commit = src.commit("calc", &[("src/lib.rs", LIB), ("src/main.rs", MAIN)]);
    let node = RepoNode::start(src.remote()).await;
    // The node serves the same source for every repository.
    for (name, ext, private) in [(REPO, "1", false), (OTHER, "2", false), (HIDDEN, "3", true)] {
        let id = insert_repo(&db, name, ext, private).await;
        node.clone_repo(id, name).await;
        if name == REPO {
            node.index_repo(id, name, &[]);
        }
    }
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
        commit,
        schema,
    })
}

impl Env {
    async fn ok(&self, query: &str, vars: Value) -> Value {
        let req = Request::new(query).variables(Variables::from_json(vars));
        let resp = sourcegull_graphql::execute(&self.schema, req, None).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn symbol_info(&self, rev: &str, path: &str, line: i32, character: i32) -> Value {
        let q = r#"query($repo: String!, $rev: String!, $path: String!, $line: Int!, $ch: Int!) {
            repository(name: $repo) { commit(rev: $rev) { blob(path: $path) {
                symbolInfo(line: $line, character: $ch) {
                    definition { repo commit path range { line character length } line character length }
                    hover
                    range { start { line character } end { line character } }
                    provenance
                }
            } } } }"#;
        let vars = json!({ "repo": REPO, "rev": rev, "path": path, "line": line, "ch": character });
        self.ok(q, vars).await["repository"]["commit"]["blob"]["symbolInfo"].clone()
    }

    async fn usages(&self, line: i32, start: i32, end: i32, extra: Value) -> Value {
        let q = r#"query($range: RangeInput!, $filter: UsagesFilter, $first: Int, $after: String) {
            usagesForSymbol(range: $range, filter: $filter, first: $first, after: $after) {
                nodes {
                    symbol { name }
                    provenance dataSource usageKind surroundingContent
                    usageRange { repository revision path range { start { line character } end { line character } } }
                }
                pageInfo { hasNextPage endCursor }
            } }"#;
        let mut vars = json!({ "range": {
            "repository": REPO, "path": "src/main.rs",
            "start": { "line": line, "character": start }, "end": { "line": line, "character": end },
        } });
        if let Value::Object(m) = extra {
            vars.as_object_mut().unwrap().extend(m);
        }
        self.ok(q, vars).await["usagesForSymbol"].clone()
    }
}

/// (repository, path, line, character, kind) of each usage.
fn summary(conn: &Value) -> Vec<(String, String, i64, i64, String)> {
    conn["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            let r = &u["usageRange"];
            (
                r["repository"].as_str().unwrap().to_owned(),
                r["path"].as_str().unwrap().to_owned(),
                r["range"]["start"]["line"].as_i64().unwrap(),
                r["range"]["start"]["character"].as_i64().unwrap(),
                u["usageKind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_based_hover_and_definition() {
    let Some(env) = setup().await else { return };
    // `add` in `add(1, 2)` on line 5 of main.rs.
    let info = env.symbol_info("main", "src/main.rs", 5, 30).await;
    assert_eq!(info["provenance"], "SEARCH_BASED");
    assert_eq!(
        info["range"],
        json!({ "start": { "line": 5, "character": 29 }, "end": { "line": 5, "character": 32 } })
    );
    let def = &info["definition"];
    assert_eq!(def["repo"], REPO);
    assert_eq!(def["commit"], env.commit.as_str());
    assert_eq!(def["path"], "src/lib.rs");
    assert_eq!(
        def["range"],
        json!({ "line": 4, "character": 7, "length": 3 })
    );
    assert_eq!(
        (&def["line"], &def["character"], &def["length"]),
        (&json!(4), &json!(7), &json!(3))
    );
    assert_eq!(
        info["hover"],
        "```rust\npub fn add(a: i32, b: i32) -> i32 {\n```\n\n---\n\nAdds two numbers.\n\nWraps on overflow."
    );

    // At a commit that is not the default branch head (unindexed search).
    let info = env
        .symbol_info(&env.commit.clone(), "src/main.rs", 6, 17)
        .await;
    assert_eq!(
        info["range"]["start"]["character"], 16,
        "UTF-16 columns after `ï`"
    );
    assert_eq!(info["definition"]["path"], "src/lib.rs");

    // Keywords, strings, comments, punctuation and unknown names.
    assert_eq!(
        env.symbol_info("main", "src/main.rs", 2, 1).await,
        Value::Null
    );
    assert_eq!(
        env.symbol_info("main", "src/main.rs", 4, 17).await,
        Value::Null
    );
    assert_eq!(
        env.symbol_info("main", "src/main.rs", 3, 8).await,
        Value::Null
    );
    assert_eq!(
        env.symbol_info("main", "src/main.rs", 2, 8).await,
        Value::Null
    );
    // A name without a definition anywhere.
    let none = env.symbol_info("main", "src/main.rs", 5, 10).await;
    assert_eq!(none["range"]["start"]["character"], 4, "{none}");
    assert_eq!(none["definition"], Value::Null, "{none}");
    assert_eq!(none["hover"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syntactic_locals() {
    let Some(env) = setup().await else { return };
    // `label` in `println!(…, label, …)` is a local of `main`.
    let info = env.symbol_info("main", "src/main.rs", 5, 23).await;
    assert_eq!(info["provenance"], "SYNTACTIC", "{info}");
    assert_eq!(
        info["range"],
        json!({ "start": { "line": 5, "character": 22 }, "end": { "line": 5, "character": 27 } })
    );
    let def = &info["definition"];
    assert_eq!(
        (&def["repo"], &def["path"]),
        (&json!(REPO), &json!("src/main.rs"))
    );
    assert_eq!(
        def["range"],
        json!({ "line": 4, "character": 8, "length": 5 })
    );
    // The comment right above a definition is its documentation.
    assert_eq!(
        info["hover"],
        "```rust\nlet label = \"add\";\n```\n\n---\n\nadd is not called in this comment"
    );

    // Its usages are the file's occurrences only, whatever other repositories hold.
    let usages = env.usages(5, 22, 27, json!({})).await;
    assert_eq!(
        summary(&usages),
        [
            (
                REPO.to_owned(),
                "src/main.rs".to_owned(),
                4,
                8,
                "DEFINITION".to_owned()
            ),
            (
                REPO.to_owned(),
                "src/main.rs".to_owned(),
                5,
                22,
                "REFERENCE".to_owned()
            ),
        ]
    );
    let u = &usages["nodes"][0];
    assert_eq!(u["provenance"], "SYNTACTIC");
    assert_eq!(u["dataSource"], "syntactic");
    assert_eq!(u["symbol"]["name"], "local 0");
    assert_eq!(u["surroundingContent"], "    let label = \"add\";");
    let page = env.usages(5, 22, 27, json!({ "first": 1 })).await;
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let rest = env
        .usages(
            5,
            22,
            27,
            json!({ "first": 1, "after": page["pageInfo"]["endCursor"] }),
        )
        .await;
    assert_eq!(summary(&rest)[0].2, 5);
    assert_eq!(rest["pageInfo"]["hasNextPage"], false);
    let other = env
        .usages(
            5,
            22,
            27,
            json!({ "filter": { "repository": { "name": { "equals": OTHER } } } }),
        )
        .await;
    assert_eq!(summary(&other), []);

    // The occurrences as code graph data.
    let q = r#"query($repo: String!, $p: CodeGraphDataProvenance) { repository(name: $repo) { commit(rev: "main") {
        blob(path: "src/main.rs") { codeGraphData(filter: { provenance: { equals: $p } }) {
            provenance commit toolInfo { name version }
            occurrences { nodes { symbol roles range { start { line character } end { line character } } } pageInfo { hasNextPage } }
            page: occurrences(first: 1, after: null) { nodes { symbol } pageInfo { hasNextPage endCursor } }
        } } } } }"#;
    let data = env.ok(q, json!({ "repo": REPO, "p": null })).await;
    let cgd = &data["repository"]["commit"]["blob"]["codeGraphData"];
    assert_eq!(cgd.as_array().unwrap().len(), 1, "{cgd}");
    let d = &cgd[0];
    assert_eq!(d["provenance"], "SYNTACTIC");
    assert_eq!(d["commit"], env.commit.as_str());
    assert_eq!(d["toolInfo"]["name"], "sourcegull-syntactic");
    let occ: Vec<(String, String, i64)> = d["occurrences"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["symbol"].as_str().unwrap().to_owned(),
                o["roles"][0].as_str().unwrap().to_owned(),
                o["range"]["start"]["line"].as_i64().unwrap(),
            )
        })
        .collect();
    let s = |sym: &str, role: &str, line| (sym.to_owned(), role.to_owned(), line);
    assert_eq!(
        occ,
        [
            s("local 0", "DEFINITION", 4),
            s("local 0", "REFERENCE", 5),
            s("local 1", "DEFINITION", 6)
        ]
    );
    assert_eq!(d["occurrences"]["pageInfo"]["hasNextPage"], false);
    assert_eq!(d["page"]["pageInfo"]["hasNextPage"], true);
    for p in ["PRECISE", "SEARCH_BASED"] {
        let data = env.ok(q, json!({ "repo": REPO, "p": p })).await;
        assert_eq!(
            data["repository"]["commit"]["blob"]["codeGraphData"],
            json!([]),
            "{p}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_based_usages() {
    let Some(env) = setup().await else { return };
    let all = env.usages(5, 29, 32, json!({})).await;
    let own = |path: &str, line, ch, kind: &str| {
        (REPO.to_owned(), path.to_owned(), line, ch, kind.to_owned())
    };
    let fork = |path: &str, line, ch, kind: &str| {
        (OTHER.to_owned(), path.to_owned(), line, ch, kind.to_owned())
    };
    // The file itself first, then the rest of its repository, then other repositories;
    // the comment and the string are not usages, and the hidden repository never appears.
    let expected = vec![
        own("src/main.rs", 0, 10, "REFERENCE"),
        own("src/main.rs", 5, 29, "REFERENCE"),
        own("src/main.rs", 6, 16, "REFERENCE"),
        own("src/lib.rs", 4, 7, "DEFINITION"),
        fork("src/lib.rs", 4, 7, "DEFINITION"),
        fork("src/main.rs", 0, 10, "REFERENCE"),
        fork("src/main.rs", 5, 29, "REFERENCE"),
        fork("src/main.rs", 6, 16, "REFERENCE"),
    ];
    assert_eq!(summary(&all), expected, "{all}");
    let first = &all["nodes"][1];
    assert_eq!(first["provenance"], "SEARCH_BASED");
    assert_eq!(first["dataSource"], "search-based");
    assert_eq!(first["symbol"]["name"], "add");
    assert_eq!(
        first["surroundingContent"],
        "    println!(\"{} {}\", label, add(1, 2));"
    );
    assert_eq!(first["usageRange"]["revision"], env.commit.as_str());
    assert_eq!(first["usageRange"]["range"]["end"]["character"], 32);
    assert_eq!(all["pageInfo"]["hasNextPage"], false);

    // Pages cover the same list.
    let mut paged = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = env
            .usages(5, 29, 32, json!({ "first": 3, "after": after }))
            .await;
        paged.extend(summary(&page));
        if page["pageInfo"]["hasNextPage"] != true {
            break;
        }
        after = page["pageInfo"]["endCursor"].clone();
    }
    assert_eq!(paged, expected);

    // Repository filters.
    let only = env
        .usages(
            5,
            29,
            32,
            json!({ "filter": { "repository": { "name": { "equals": OTHER } } } }),
        )
        .await;
    assert_eq!(summary(&only), expected[4..]);
    let not = env
        .usages(
            5,
            29,
            32,
            json!({ "filter": { "not": { "repository": { "name": { "equals": OTHER } } } } }),
        )
        .await;
    assert_eq!(summary(&not), expected[..4]);
    let hidden = env
        .usages(
            5,
            29,
            32,
            json!({ "filter": { "repository": { "name": { "equals": HIDDEN } } } }),
        )
        .await;
    assert_eq!(summary(&hidden), []);

    // Not an identifier: no usages.
    let none = env.usages(2, 1, 3, json!({})).await;
    assert_eq!(summary(&none), []);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usages_of_a_hidden_repository_are_not_found() {
    let Some(env) = setup().await else { return };
    let q =
        r#"query($range: RangeInput!) { usagesForSymbol(range: $range) { nodes { usageKind } } }"#;
    let vars = json!({ "range": {
        "repository": HIDDEN, "path": "src/main.rs",
        "start": { "line": 5, "character": 29 }, "end": { "line": 5, "character": 32 },
    } });
    let req = Request::new(q).variables(Variables::from_json(vars.clone()));
    let resp = sourcegull_graphql::execute(&env.schema, req, None).await;
    let hidden = resp.errors[0].message.replace(HIDDEN, "X");
    let mut missing = vars;
    missing["range"]["repository"] = json!("gitlab.example.com/acme/missing");
    let req = Request::new(q).variables(Variables::from_json(missing));
    let resp = sourcegull_graphql::execute(&env.schema, req, None).await;
    assert_eq!(
        resp.errors[0]
            .message
            .replace("gitlab.example.com/acme/missing", "X"),
        hidden
    );
}
