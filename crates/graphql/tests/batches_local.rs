//! What local runs (`gull bulk …`) and remote runs of command-line clients
//! need besides uploads, against the test Postgres and an in-process repo
//! node: `resolveWorkspacesForBatchSpec` with a client's operation (query and explicit
//! repositories, `rootAtLocationOf` workspaces, unsupported and ignored repositories flagged,
//! archived and unreadable ones left out, validation errors, access), `BatchSpec.files`, and
//! the workspace resolution a remote run polls.

// Tests seed repository rows directly.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use chrono::Utc;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_batches::store::{
    BatchSpecStore, BatchSpecWorkspaceFileStore, NewBatchSpec, NewBatchSpecWorkspaceFile,
};
use sourcegull_config::{ConfigHandle, SiteConfig, SiteConfigHandle};
use sourcegull_db::{Db, Namespace, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_string_id;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{RepoNode, SourceRepo};
use sourcegull_types::{RepoId, UserId};

const WEB: &str = "gitlab.example.com/acme/web";
const API: &str = "gitlab.example.com/acme/api";
const OLD: &str = "gitlab.example.com/acme/old";
const SECRET: &str = "gitlab.example.com/acme/secret";
const MIRROR: &str = "github.com/acme/mirror";
const TODO: &str = "// TODO fixme\n";

/// The `ResolveWorkspacesForBatchSpec` operation of command-line clients.
const RESOLVE: &str = "query ResolveWorkspacesForBatchSpec($spec: String!) {
    resolveWorkspacesForBatchSpec(batchSpec: $spec) {
        onlyFetchWorkspace
        ignored
        unsupported
        repository {
            id
            name
            url
            externalRepository { serviceType }
            defaultBranch { name target { oid } }
        }
        branch {
            abbrevName
            name
            target { oid }
        }
        path
        searchResultPaths
    }
}";

struct Env {
    _t: TestDb,
    _node: RepoNode,
    _sources: Vec<SourceRepo>,
    db: Db,
    auth: Authenticator,
    config: SiteConfigHandle,
    schema: SourcegullSchema,
    heads: HashMap<&'static str, String>,
    alice: UserId,
    bob: UserId,
}

impl Env {
    async fn run(&self, user: Option<UserId>, query: &str, vars: Value) -> Response {
        let actor = match user {
            Some(u) => Some(
                self.auth
                    .actor_for(u, AuthVia::Session)
                    .await
                    .unwrap()
                    .unwrap(),
            ),
            None => None,
        };
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(Some(user), query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, user: Option<UserId>, query: &str, vars: Value) -> (String, Value) {
        let resp = self.run(user, query, vars).await;
        let e = resp.errors.first().expect("an error");
        let ext = e
            .extensions
            .as_ref()
            .map(|x| serde_json::to_value(x).unwrap())
            .unwrap_or_default();
        (ext["code"].as_str().unwrap_or_default().to_owned(), ext)
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    let mut remotes = HashMap::new();
    let mut heads = HashMap::new();
    let mut sources = Vec::new();
    for name in [WEB, API, OLD, SECRET, MIRROR] {
        let s = SourceRepo::new();
        let files: Vec<(&str, &str)> = match name {
            WEB => vec![
                ("README.md", "web\n"),
                ("packages/a/package.json", "{}\n"),
                ("packages/a/index.js", TODO),
                ("packages/b/package.json", "{}\n"),
                ("src/app.js", TODO),
            ],
            API => vec![("main.go", TODO), (".batchignore", "")],
            _ => vec![("main.go", TODO)],
        };
        s.commit(name, &files);
        heads.insert(name, s.head());
        remotes.insert(name.to_owned(), s.remote());
        sources.push(s);
    }
    let node = RepoNode::start_with(move |r| Ok(remotes[&r.name].clone())).await;
    for (i, name) in [WEB, API, OLD, SECRET, MIRROR].into_iter().enumerate() {
        let service = if name == MIRROR { "github" } else { "gitlab" };
        let id: i32 = sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, external_id, external_service_type, external_service_id, \
                 archived, private) \
             VALUES ($1, $2, $3, 'https://code.example.com/', $4, $5) RETURNING id",
        )
        .bind(name)
        .bind(i.to_string())
        .bind(service)
        .bind(name == OLD)
        .bind(name == SECRET)
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
    let user = |name: &'static str| {
        let db = db.clone();
        async move {
            db.users()
                .create(&NewUser {
                    username: name.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .id
        }
    };
    let alice = user("alice").await;
    let bob = user("bob").await;
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: node.clients.clone(),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    Some(Env {
        _t: t,
        _node: node,
        _sources: sources,
        db,
        auth,
        config,
        schema,
        heads,
        alice,
        bob,
    })
}

const STEPS: &str = "steps:
  - run: echo hi
    container: alpine:3
changesetTemplate:
  title: t
  branch: b
  commit:
    message: m
";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_workspaces_for_batch_spec() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    // A query finds web, api (ignored) and the GitHub mirror (unsupported); the archived and
    // the private repositories are left out.
    let spec =
        format!("version: 2\nname: q\non:\n  - repositoriesMatchingQuery: TODO fixme\n{STEPS}");
    let data = env.ok(env.alice, RESOLVE, json!({ "spec": spec })).await;
    let ws = data["resolveWorkspacesForBatchSpec"].as_array().unwrap();
    let mut got: Vec<(&str, bool, bool)> = ws
        .iter()
        .map(|w| {
            (
                w["repository"]["name"].as_str().unwrap(),
                w["ignored"].as_bool().unwrap(),
                w["unsupported"].as_bool().unwrap(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            (MIRROR, false, true),
            (API, true, false),
            (WEB, false, false)
        ]
    );
    let web = ws.iter().find(|w| w["repository"]["name"] == WEB).unwrap();
    assert_eq!(web["path"], "");
    assert_eq!(web["onlyFetchWorkspace"], false);
    assert_eq!(
        web["searchResultPaths"],
        json!(["packages/a/index.js", "src/app.js"])
    );
    assert_eq!(web["branch"]["name"], "refs/heads/main");
    assert_eq!(web["branch"]["abbrevName"], "main");
    assert_eq!(web["branch"]["target"]["oid"], env.heads[WEB].as_str());
    assert_eq!(web["repository"]["url"], format!("/{WEB}"));
    assert_eq!(
        web["repository"]["externalRepository"]["serviceType"],
        "gitlab"
    );
    assert_eq!(
        web["repository"]["defaultBranch"]["name"],
        "refs/heads/main"
    );
    assert_eq!(
        web["repository"]["defaultBranch"]["target"]["oid"],
        env.heads[WEB].as_str()
    );

    // rootAtLocationOf with onlyFetchWorkspace, on an explicit repository.
    let spec = format!(
        "name: w\non:\n  - repository: {WEB}\nworkspaces:\n  - rootAtLocationOf: package.json\n    \
         onlyFetchWorkspace: true\n{STEPS}"
    );
    let data = env.ok(env.alice, RESOLVE, json!({ "spec": spec })).await;
    let paths: Vec<(&str, bool)> = data["resolveWorkspacesForBatchSpec"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["path"].as_str().unwrap(),
                w["onlyFetchWorkspace"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(paths, [("packages/a", true), ("packages/b", true)]);

    // An explicit private repository is not found for alice.
    let spec = format!("name: s\non:\n  - repository: {SECRET}\n{STEPS}");
    let (code, _) = env
        .err(Some(env.alice), RESOLVE, json!({ "spec": spec }))
        .await;
    assert_eq!(code, "INVALID_INPUT");

    // A spec without steps has no workspaces.
    let spec = format!("name: n\non:\n  - repository: {WEB}\n");
    let data = env.ok(env.alice, RESOLVE, json!({ "spec": spec })).await;
    assert_eq!(data["resolveWorkspacesForBatchSpec"], json!([]));

    // Validation errors carry positions; anonymous callers and disabled bulk changes fail.
    let (code, ext) = env
        .err(
            Some(env.alice),
            RESOLVE,
            json!({ "spec": "name: x\nsteps: 3\n" }),
        )
        .await;
    assert_eq!(code, "VALIDATION");
    assert_eq!(ext["issues"][0]["line"], 2, "{ext}");
    let spec = format!("name: q\non:\n  - repository: {WEB}\n{STEPS}");
    let (code, _) = env.err(None, RESOLVE, json!({ "spec": spec })).await;
    assert_eq!(code, "UNAUTHORIZED");
    env.config.store(
        2,
        SiteConfig {
            batch_changes_enabled: false,
            ..SiteConfig::default()
        },
    );
    let (code, _) = env
        .err(Some(env.alice), RESOLVE, json!({ "spec": spec }))
        .await;
    assert_eq!(code, "ErrBatchChangesDisabled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spec_files_and_resolution_polling() {
    let Some(env) = setup().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let raw = format!("name: f\non:\n  - repository: {WEB}\n{STEPS}");
    let spec = BatchSpecStore::new(&env.db)
        .create(&NewBatchSpec {
            raw_spec: raw.clone(),
            spec: serde_json::to_value(sourcegull_batches::BatchSpec::parse(&raw).unwrap())
                .unwrap(),
            namespace: Namespace::User(env.alice),
            user_id: Some(env.alice),
            created_from_raw: true,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .unwrap();
    BatchSpecStore::new(&env.db)
        .enqueue_resolution(spec.id, env.alice)
        .await
        .unwrap();
    let files = BatchSpecWorkspaceFileStore::new(&env.db);
    for (path, name, content) in [
        ("scripts", "fix.sh", b"echo fix\n".as_slice()),
        ("", "data.bin", b"\x00\xff".as_slice()),
    ] {
        files
            .upsert(&NewBatchSpecWorkspaceFile {
                batch_spec_id: spec.id,
                path,
                filename: name,
                content,
                modified_at: Utc::now(),
            })
            .await
            .unwrap();
    }
    let id = marshal_string_id("BatchSpec", &spec.rand_id);

    // A client's poll of a remote run's resolution.
    let data = env
        .ok(
            env.alice,
            "query BatchSpecWorkspaceResolution($batchSpec: ID!) {
                node(id: $batchSpec) {
                    ... on BatchSpec {
                        workspaceResolution { failureMessage state }
                    }
                }
            }",
            json!({ "batchSpec": id }),
        )
        .await;
    assert_eq!(
        data["node"]["workspaceResolution"],
        json!({"failureMessage": null, "state": "QUEUED"})
    );

    const FILES: &str = "query($id: ID!, $after: String) {
        node(id: $id) {
            ... on BatchSpec {
                files(first: 1, after: $after) {
                    totalCount
                    pageInfo { hasNextPage endCursor }
                    nodes { id path name isDirectory byteSize binary content url modifiedAt }
                }
            }
        }
    }";
    let data = env.ok(env.alice, FILES, json!({ "id": id })).await;
    let page = &data["node"]["files"];
    assert_eq!(page["totalCount"], 2);
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let first = &page["nodes"][0];
    assert_eq!(first["path"], "data.bin");
    assert_eq!(first["binary"], true);
    assert_eq!(first["byteSize"], 2);
    let data = env
        .ok(
            env.alice,
            FILES,
            json!({ "id": id, "after": page["pageInfo"]["endCursor"] }),
        )
        .await;
    let page = &data["node"]["files"];
    assert_eq!(page["pageInfo"]["hasNextPage"], false);
    let fix = &page["nodes"][0];
    assert_eq!(fix["path"], "scripts/fix.sh");
    assert_eq!(fix["name"], "fix.sh");
    assert_eq!(fix["isDirectory"], false);
    assert_eq!(fix["binary"], false);
    assert_eq!(fix["content"], "echo fix\n");
    let rand = files
        .get_by_name(spec.id, "scripts", "fix.sh")
        .await
        .unwrap()
        .unwrap()
        .rand_id;
    assert_eq!(
        fix["url"],
        format!("/.api/files/batch-changes/{}/{rand}", spec.rand_id)
    );
    // Others see the spec (its ID is the secret) but not its files.
    let data = env.ok(env.bob, FILES, json!({ "id": id })).await;
    assert_eq!(data["node"]["files"], Value::Null);
}
