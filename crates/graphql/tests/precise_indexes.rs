//! Precise indexes over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): listing and filters honour repository permissions, and site
//! admins delete and reindex.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_codeintel_scip::store::{CodeIntelStore, NewUpload, UploadState};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::{marshal_id, marshal_string_id};
use sourcegull_testutil::TestDb;
use sourcegull_types::{RepoId, UserId};

const PUBLIC: &str = "gitlab.example.com/acme/public";
const PRIVATE: &str = "gitlab.example.com/acme/private";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    public: RepoId,
    private: RepoId,
}

async fn insert_repo(db: &Db, name: &str, ext: &str, private: bool) -> RepoId {
    RepoId(
        sourcegull_db::sqlx::query_scalar(
            "INSERT INTO repo (name, private, external_id, external_service_type,
                external_service_id)
             VALUES ($1, $2, $3, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
        )
        .bind(name)
        .bind(private)
        .bind(ext)
        .fetch_one(db.pool())
        .await
        .unwrap(),
    )
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, SiteConfig::default());
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let admin = auth
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let public = insert_repo(&db, PUBLIC, "1", false).await;
    let private = insert_repo(&db, PRIVATE, "2", true).await;
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            sourcegull_rpc::Router::default(),
            sourcegull_rpc::ClientConfig::default(),
        )),
        authz: Checker::new(db.clone(), config),
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        auth,
        schema,
        admin,
        alice,
        public,
        private,
    })
}

impl Env {
    /// An upload of `repo` in `state` (with a stored index for completed and failed ones).
    async fn upload(&self, repo: RepoId, root: &str, indexer: &str, state: &str) -> i64 {
        let store = CodeIntelStore::new(&self.db);
        let id = store
            .create_upload(&NewUpload {
                repo_id: repo,
                commit: COMMIT.into(),
                root: root.into(),
                indexer: indexer.into(),
                indexer_version: "1.0".into(),
                num_parts: 1,
                uploader: None,
                associated_index_id: None,
                ci_job: None,
            })
            .await
            .unwrap();
        sourcegull_db::sqlx::query(
            "UPDATE codeintel_uploads SET state = $2,
                upload_size = CASE WHEN $2 = 'uploading' THEN NULL ELSE 100 END,
                failure_message = CASE WHEN $2 = 'failed' THEN 'boom' END
             WHERE id = $1",
        )
        .bind(id)
        .bind(state)
        .execute(self.db.pool())
        .await
        .unwrap();
        id
    }

    async fn run(&self, actor: UserId, query: &str, vars: Value) -> Response {
        let actor = self.auth.actor_for(actor, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, actor: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn code(&self, actor: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(actor, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        err.extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// `(inputRoot, state)` of the indexes `actor` lists with `vars`, and the total count.
    async fn list(&self, actor: UserId, vars: Value) -> (Vec<(String, String)>, i64) {
        let data = self.ok(actor, LIST, vars).await;
        let c = &data["preciseIndexes"];
        let rows = c["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                (
                    n["inputRoot"].as_str().unwrap().to_owned(),
                    n["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        (rows, c["totalCount"].as_i64().unwrap())
    }

    async fn state(&self, id: i64) -> UploadState {
        CodeIntelStore::new(&self.db)
            .get_upload(id)
            .await
            .unwrap()
            .unwrap()
            .state
    }
}

fn index_id(id: i64) -> String {
    marshal_string_id("PreciseIndex", &format!("U:{id}"))
}

fn repo_id(id: RepoId) -> String {
    marshal_id("Repository", i64::from(id.get()))
}

const LIST: &str = "query($repo: ID, $query: String, $states: [PreciseIndexState!],
        $key: String, $deleted: Boolean, $first: Int, $after: String) {
    preciseIndexes(repo: $repo, query: $query, states: $states, indexerKey: $key,
            includeDeleted: $deleted, first: $first, after: $after) {
        totalCount pageInfo { hasNextPage endCursor }
        nodes { inputRoot state } } }";
const DELETE: &str = "mutation($id: ID!) { deletePreciseIndex(id: $id) { alwaysNil } }";
const REINDEX: &str = "mutation($id: ID!) { reindexPreciseIndex(id: $id) { alwaysNil } }";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_indexes_the_viewer_can_read() {
    let Some(env) = setup().await else { return };
    let done = env.upload(env.public, "", "scip-go", "completed").await;
    env.upload(env.public, "web/", "sourcegraph/scip-typescript", "failed")
        .await;
    env.upload(env.public, "queued/", "scip-go", "queued").await;
    let hidden = env.upload(env.private, "", "scip-java", "completed").await;

    // Alice cannot read the private repository: its index is not listed or found.
    let (rows, total) = env.list(env.alice, json!({})).await;
    assert_eq!(total, 3);
    assert_eq!(rows.len(), 3);
    assert!(!rows.iter().any(|(_, s)| s == "DELETED"));
    let (rows, total) = env
        .list(env.alice, json!({ "repo": repo_id(env.private) }))
        .await;
    assert_eq!((rows.len(), total), (0, 0));
    let node = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { id } }",
            json!({ "id": index_id(hidden) }),
        )
        .await;
    assert!(node["node"].is_null());
    let keys = env.ok(env.alice, "query { indexerKeys }", json!({})).await;
    assert_eq!(keys["indexerKeys"], json!([]));

    // The admin sees all four.
    let (_, total) = env.list(env.admin, json!({})).await;
    assert_eq!(total, 4);
    let keys = env.ok(env.admin, "query { indexerKeys }", json!({})).await;
    assert_eq!(keys["indexerKeys"], json!(["go", "java", "typescript"]));
    let keys = env
        .ok(
            env.alice,
            "query($r: ID) { indexerKeys(repo: $r) }",
            json!({ "r": repo_id(env.public) }),
        )
        .await;
    assert_eq!(keys["indexerKeys"], json!(["go", "typescript"]));

    // Filters: state, indexer key (by image name too), query over the repository name,
    // pagination.
    let (rows, _) = env
        .list(env.alice, json!({ "states": ["PROCESSING_ERRORED"] }))
        .await;
    assert_eq!(rows, [("web/".to_owned(), "PROCESSING_ERRORED".to_owned())]);
    let (rows, _) = env.list(env.alice, json!({ "key": "typescript" })).await;
    assert_eq!(rows.len(), 1);
    let (_, total) = env.list(env.admin, json!({ "query": "acme/PRIV" })).await;
    assert_eq!(total, 1);
    let (_, total) = env.list(env.admin, json!({ "query": "0123456789" })).await;
    assert_eq!(total, 4);
    let (_, total) = env
        .list(env.alice, json!({ "states": ["QUEUED_FOR_INDEXING"] }))
        .await;
    assert_eq!(total, 0);
    let page = env.ok(env.alice, LIST, json!({ "first": 2 })).await;
    let info = &page["preciseIndexes"]["pageInfo"];
    assert_eq!(info["hasNextPage"], json!(true));
    let (rows, _) = env
        .list(env.alice, json!({ "first": 2, "after": info["endCursor"] }))
        .await;
    assert_eq!(rows.len(), 1);

    // Fields of one index.
    let data = env
        .ok(
            env.alice,
            "query($id: ID!) { node(id: $id) { ... on PreciseIndex {
                id inputCommit inputIndexer state failure placeInQueue uploadedAt
                indexer { key name url imageName }
                projectRoot { path url commit { oid abbreviatedOID url }
                              repository { name url } } } } }",
            json!({ "id": index_id(done) }),
        )
        .await;
    let n = &data["node"];
    assert_eq!(n["id"], json!(index_id(done)));
    assert_eq!(n["state"], json!("COMPLETED"));
    assert_eq!(n["placeInQueue"], Value::Null);
    assert_eq!(
        n["indexer"],
        json!({ "key": "go", "name": "scip-go",
                "url": "https://github.com/sourcegraph/scip-go",
                "imageName": "sourcegraph/scip-go" })
    );
    let root = &n["projectRoot"];
    assert_eq!(root["path"], json!(""));
    assert_eq!(root["url"], json!(format!("/{PUBLIC}@{COMMIT}")));
    assert_eq!(root["commit"]["abbreviatedOID"], json!("0123456"));
    assert_eq!(root["repository"]["url"], json!(format!("/{PUBLIC}")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn site_admins_delete_and_reindex() {
    let Some(env) = setup().await else { return };
    let done = env.upload(env.public, "", "scip-go", "completed").await;
    let failed = env
        .upload(env.public, "web/", "scip-typescript", "failed")
        .await;
    let uploading = env.upload(env.public, "up/", "scip-go", "uploading").await;
    let other = env.upload(env.private, "", "scip-java", "completed").await;

    assert_eq!(
        env.code(env.alice, DELETE, json!({ "id": index_id(done) }))
            .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(env.alice, REINDEX, json!({ "id": index_id(done) }))
            .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.code(env.admin, DELETE, json!({ "id": index_id(999_999) }))
            .await,
        "NOT_FOUND"
    );

    // Reindexing queues completed and failed indexes again; others are refused.
    env.ok(env.admin, REINDEX, json!({ "id": index_id(failed) }))
        .await;
    assert_eq!(env.state(failed).await, UploadState::Queued);
    assert_eq!(
        env.code(env.admin, REINDEX, json!({ "id": index_id(uploading) }))
            .await,
        "INVALID_INPUT"
    );

    env.ok(env.admin, DELETE, json!({ "id": index_id(done) }))
        .await;
    assert_eq!(env.state(done).await, UploadState::Deleting);
    // Deleted indexes are listed only on request.
    let (rows, _) = env
        .list(env.admin, json!({ "repo": repo_id(env.public) }))
        .await;
    assert_eq!(rows.len(), 2);
    let (rows, _) = env
        .list(
            env.admin,
            json!({ "repo": repo_id(env.public), "deleted": true }),
        )
        .await;
    assert_eq!(rows.len(), 3);
    let (rows, _) = env.list(env.admin, json!({ "states": ["DELETING"] })).await;
    assert_eq!(rows, [(String::new(), "DELETING".to_owned())]);

    // Bulk: reindex and delete by filter.
    env.ok(
        env.admin,
        "mutation($r: ID) { reindexPreciseIndexes(repository: $r) { alwaysNil } }",
        json!({ "r": repo_id(env.private) }),
    )
    .await;
    assert_eq!(env.state(other).await, UploadState::Queued);
    env.ok(
        env.admin,
        "mutation { deletePreciseIndexes(states: [UPLOADING_INDEX]) { alwaysNil } }",
        json!({}),
    )
    .await;
    assert_eq!(env.state(uploading).await, UploadState::Deleting);
    assert_eq!(env.state(other).await, UploadState::Queued);
    assert_eq!(env.state(failed).await, UploadState::Queued);
}
