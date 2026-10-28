//! Site admin repository GraphQL: the `repositories` filters for corrupt and indexed
//! repos, `repositoryStats`, `Repository.textSearchIndex`, `gitservers` (disk usage per repo
//! node), `mirrorInfo` and the `recloneRepository` / `reindexRepository` mutations, against the
//! test Postgres and a repo node started in-process. Everything is site admins only.

// Tests read repository rows directly (clippy.toml forbids it in product code).
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, FetchResult, IndexedBranchRow, IndexedState, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_repos::Registry;
use sourcegull_search::index::SearchIndex;
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_testutil::git::{BrowseRepoOids, RepoNode, SourceRepo, browse_repo};
use sourcegull_types::{RemoteUnavailable, RepoId, UserId};

const DEMO: &str = "gitlab.example.com/acme/demo";
const BROKEN: &str = "gitlab.example.com/acme/broken";
const CORRUPT: &str = "gitlab.example.com/acme/corrupt";
const UNCLONED: &str = "gitlab.example.com/acme/uncloned";
const MISSING_LAST_ERROR: &str = "git fetch failed (exit status: 128): remote: A repository for \
     this project does not exist yet.";
/// A repo node that left the node list but still has a repo assigned (nothing listens here).
const GONE: &str = "http://127.0.0.1:1";

struct Env {
    _t: TestDb,
    src: SourceRepo,
    node: RepoNode,
    oids: BrowseRepoOids,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    ids: HashMap<&'static str, RepoId>,
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
    sqlx::query("INSERT INTO gitserver_repos (repo_id) VALUES ($1)")
        .bind(id)
        .execute(db.pool())
        .await
        .unwrap();
    RepoId(id)
}

async fn setup() -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate");
    let db = Db::from_pool(tdb.pool.clone());
    let mut site = SiteConfig::default();
    site.search_index_branches
        .insert(DEMO.to_owned(), vec!["feature".to_owned()]);
    let config = ConfigHandle::new(1, site);
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

    let (src, oids) = browse_repo();
    let node = RepoNode::start(src.remote()).await;
    let mut ids = HashMap::new();
    for (i, name) in [DEMO, BROKEN, CORRUPT, UNCLONED].into_iter().enumerate() {
        ids.insert(name, insert_repo(&db, name, &i.to_string()).await);
    }
    let gs = db.gitserver_repos();
    let fetched = |size| FetchResult {
        changed: true,
        head_oid: Some("0".repeat(40)),
        size_bytes: Some(size),
        node: node.addr.clone(),
        remote_unavailable: None,
    };
    // DEMO: cloned and indexed (HEAD and `feature`) on the node.
    node.clone_repo(ids[DEMO], DEMO).await;
    node.index_repo(ids[DEMO], DEMO, &["feature"]);
    gs.record_fetched(ids[DEMO], &fetched(1000)).await.unwrap();
    let status = node.index.repo_status(ids[DEMO]).unwrap();
    db.search_index()
        .record_indexed(
            ids[DEMO],
            &node.addr,
            &IndexedState {
                branches: status
                    .branches
                    .iter()
                    .map(|b| IndexedBranchRow {
                        name: b.name.clone(),
                        commit: b.commit.clone(),
                    })
                    .collect(),
                docs: status.docs as i64,
                content_bytes: status.content_bytes as i64,
                index_bytes: status.index_bytes as i64,
                lines: status.lines as i64,
                default_branch_lines: status.default_branch_lines as i64,
                shards: status.shards.len() as i32,
                indexed_at: None,
            },
        )
        .await
        .unwrap();
    // BROKEN: the first clone failed, and so did an index build.
    gs.record_error(ids[BROKEN], &node.addr, "fatal: repository not found")
        .await
        .unwrap();
    db.search_index()
        .record_error(ids[BROKEN], &node.addr, "git: object missing")
        .await
        .unwrap();
    // CORRUPT: cloned (per the database) and found corrupt.
    gs.record_fetched(ids[CORRUPT], &fetched(500))
        .await
        .unwrap();
    gs.set_corrupted(ids[CORRUPT], "fsck failed during maintenance")
        .await
        .unwrap();
    // UNCLONED: assigned to a node that is gone.
    sqlx::query("UPDATE repo SET shard = $2 WHERE id = $1")
        .bind(ids[UNCLONED])
        .bind(GONE)
        .execute(db.pool())
        .await
        .unwrap();

    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Registry::default(),
        repo_clients: node.clients.clone(),
        authz: sourcegull_authz::Checker::new(db.clone(), config),
        app: None,
    });
    Some(Env {
        _t: tdb,
        src,
        node,
        oids,
        db,
        auth,
        schema,
        admin,
        alice,
        ids,
    })
}

impl Env {
    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
        let actor = self.auth.actor_for(user, AuthVia::Session).await.unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    async fn ok(&self, query: &str, vars: Value) -> Value {
        let resp = self.run(self.admin, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn forbidden(&self, query: &str, vars: Value) {
        let resp = self.run(self.alice, query, vars).await;
        let codes: Vec<_> = resp
            .errors
            .iter()
            .filter_map(|e| e.extensions.as_ref()?.get("code").cloned())
            .collect();
        assert!(
            codes.contains(&async_graphql::Value::from("FORBIDDEN")),
            "{query}: {:?}",
            resp.errors
        );
    }

    fn gql_id(&self, name: &str) -> String {
        marshal_id("Repository", i64::from(self.ids[name].get()))
    }
}

const LIST: &str = "query($corrupted: Boolean, $indexed: Boolean, $notIndexed: Boolean,
        $failedFetch: Boolean, $cloneStatus: CloneStatus, $query: String, $first: Int,
        $after: String) {
    repositories(corrupted: $corrupted, indexed: $indexed, notIndexed: $notIndexed,
            failedFetch: $failedFetch, cloneStatus: $cloneStatus, query: $query, first: $first,
            after: $after) {
        totalCount
        pageInfo { hasNextPage endCursor }
        nodes { name mirrorInfo { cloned isCorrupted lastError byteSize shard } }
    }
}";

fn names(data: &Value) -> Vec<&str> {
    data["repositories"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn repositories_filters_and_stats() {
    let Some(env) = setup().await else { return };
    let list = |vars: Value| env.ok(LIST, vars);

    let all = list(json!({})).await;
    assert_eq!(all["repositories"]["totalCount"], 4);
    let corrupt = list(json!({ "corrupted": true })).await;
    assert_eq!(names(&corrupt), [CORRUPT]);
    let m = &corrupt["repositories"]["nodes"][0]["mirrorInfo"];
    assert_eq!(m["isCorrupted"], true);
    assert_eq!(m["cloned"], true);
    assert_eq!(m["byteSize"], "500");
    assert_eq!(m["shard"], env.node.addr.as_str());
    assert_eq!(names(&list(json!({ "notIndexed": false })).await), [DEMO]);
    assert_eq!(
        names(&list(json!({ "indexed": false })).await),
        [BROKEN, CORRUPT, UNCLONED]
    );
    assert_eq!(
        list(json!({ "indexed": false, "notIndexed": false })).await["repositories"]["totalCount"],
        0
    );
    // Filters combine: failed fetches (the corrupt repo's finding is its last error too).
    assert_eq!(
        names(&list(json!({ "failedFetch": true })).await),
        [BROKEN, CORRUPT]
    );
    assert_eq!(
        names(&list(json!({ "failedFetch": true, "indexed": false, "query": "brok" })).await),
        [BROKEN]
    );
    assert_eq!(
        names(&list(json!({ "cloneStatus": "NOT_CLONED" })).await),
        [BROKEN, UNCLONED]
    );
    // Pagination.
    let page = list(json!({ "first": 3 })).await;
    assert_eq!(names(&page), [BROKEN, CORRUPT, DEMO]);
    assert_eq!(page["repositories"]["pageInfo"]["hasNextPage"], true);
    let cursor = page["repositories"]["pageInfo"]["endCursor"].clone();
    let rest = list(json!({ "first": 3, "after": cursor })).await;
    assert_eq!(names(&rest), [UNCLONED]);
    assert_eq!(rest["repositories"]["pageInfo"]["hasNextPage"], false);

    // Admin-only filters.
    env.forbidden(LIST, json!({ "corrupted": true })).await;
    env.forbidden(LIST, json!({ "indexed": false })).await;

    let stats = env
        .ok(
            "{ repositoryStats { total cloned cloning notCloned failedFetch indexed corrupted
                embedded gitDirBytes indexedLinesCount } }",
            json!({}),
        )
        .await;
    let s = &stats["repositoryStats"];
    assert_eq!(s["total"], 4);
    assert_eq!(s["cloned"], 2);
    assert_eq!(s["cloning"], 0);
    assert_eq!(s["notCloned"], 2);
    assert_eq!(s["failedFetch"], 2);
    assert_eq!(s["indexed"], 1);
    assert_eq!(s["corrupted"], 1);
    assert_eq!(s["embedded"], 0);
    assert_eq!(s["gitDirBytes"], "1500");
    let lines: i64 = s["indexedLinesCount"].as_str().unwrap().parse().unwrap();
    assert!(lines > 0);
    env.forbidden("{ repositoryStats { total } }", json!({}))
        .await;

    // Corruption log and the fetch schedule.
    let data = env
        .ok(
            "query($name: String!) { repository(name: $name) { mirrorInfo {
                corruptionLogs { timestamp reason } nextSyncAt } } }",
            json!({ "name": CORRUPT }),
        )
        .await;
    let m = &data["repository"]["mirrorInfo"];
    assert_eq!(
        m["corruptionLogs"][0]["reason"],
        "fsck failed during maintenance"
    );
    assert_eq!(m["nextSyncAt"], Value::Null);
}

const INDEX: &str = "query($name: String!) { repository(name: $name) { textSearchIndex {
    repository { name }
    status { updatedAt contentByteSize contentFilesCount indexByteSize indexShardsCount
             newLinesCount defaultBranchNewLinesCount otherBranchesNewLinesCount }
    refs { ref { name displayName target { oid } } indexed current indexedCommit { oid }
           indexName }
    host { name }
    lastError lastErrorAt queued nodeError
    shards { kind file byteSize filesCount repositoriesCount }
} } }";

#[tokio::test]
async fn text_search_index_and_reindex() {
    let Some(env) = setup().await else { return };
    let o = &env.oids;
    let data = env.ok(INDEX, json!({ "name": DEMO })).await;
    let ix = &data["repository"]["textSearchIndex"];
    assert_eq!(ix["repository"]["name"], DEMO);
    assert_eq!(ix["host"]["name"], env.node.addr.as_str());
    assert_eq!(ix["queued"], false);
    assert_eq!(ix["lastError"], Value::Null);
    assert_eq!(ix["nodeError"], Value::Null);
    let st = &ix["status"];
    assert!(st["contentFilesCount"].as_i64().unwrap() >= 5);
    assert_eq!(st["indexShardsCount"], 1);
    assert!(st["indexByteSize"].as_i64().unwrap() > 0);
    let lines = st["newLinesCount"].as_i64().unwrap();
    let default_lines = st["defaultBranchNewLinesCount"].as_i64().unwrap();
    assert!(default_lines > 0 && lines > default_lines, "{st}");
    assert_eq!(
        st["otherBranchesNewLinesCount"].as_i64().unwrap(),
        lines - default_lines
    );
    assert_eq!(
        ix["shards"],
        json!([{
            "kind": "base",
            "file": format!("{}.gullshard", env.ids[DEMO].get()),
            "byteSize": st["indexByteSize"].to_string(),
            "filesCount": st["contentFilesCount"],
            "repositoriesCount": 1,
        }])
    );
    let refs = ix["refs"].as_array().unwrap();
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert_eq!(refs[0]["indexName"], "HEAD");
    assert_eq!(refs[0]["ref"]["name"], "refs/heads/main");
    assert_eq!(refs[0]["indexed"], true);
    assert_eq!(refs[0]["current"], true);
    assert_eq!(refs[0]["indexedCommit"]["oid"], o.c3.as_str());
    assert_eq!(refs[1]["indexName"], "feature");
    assert_eq!(refs[1]["ref"]["displayName"], "feature");
    assert_eq!(refs[1]["indexedCommit"]["oid"], o.f1.as_str());
    assert_eq!(refs[1]["current"], true);

    // A new commit fetched but not indexed yet: HEAD's index is stale.
    let c4 = env.src.commit("c4", &[("new.txt", "new\n")]);
    env.node.fetch_repo(env.ids[DEMO], DEMO).await;
    let data = env.ok(INDEX, json!({ "name": DEMO })).await;
    let head = &data["repository"]["textSearchIndex"]["refs"][0];
    assert_eq!(head["indexed"], true);
    assert_eq!(head["current"], false);
    assert_eq!(head["ref"]["target"]["oid"], c4.as_str());
    assert_eq!(head["indexedCommit"]["oid"], o.c3.as_str());

    // Reindex: the request reaches the node, which then reports the build as queued.
    let reindex = "mutation($id: ID!) { reindexRepository(repository: $id) { alwaysNil } }";
    env.ok(reindex, json!({ "id": env.gql_id(DEMO) })).await;
    let data = env.ok(INDEX, json!({ "name": DEMO })).await;
    assert_eq!(data["repository"]["textSearchIndex"]["queued"], true);
    assert_eq!(env.node.reindex_requests.take(), [env.ids[DEMO]]);
    env.forbidden(reindex, json!({ "id": env.gql_id(DEMO) }))
        .await;

    // Not indexed: no status, the default branch listed as not indexed, the last error.
    let data = env.ok(INDEX, json!({ "name": BROKEN })).await;
    let ix = &data["repository"]["textSearchIndex"];
    assert_eq!(ix["status"], Value::Null);
    assert_eq!(ix["lastError"], "git: object missing");
    assert!(ix["lastErrorAt"].is_string());
    assert_eq!(ix["shards"], json!([]));
    let refs = ix["refs"].as_array().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0]["indexName"], "HEAD");
    assert_eq!(refs[0]["indexed"], false);
    assert_eq!(refs[0]["current"], false);
    assert_eq!(refs[0]["indexedCommit"], Value::Null);

    let resp = env.run(env.alice, INDEX, json!({ "name": DEMO })).await;
    assert!(!resp.errors.is_empty(), "site admins only");
}

#[tokio::test]
async fn reclone_queues_a_fresh_clone() {
    let Some(env) = setup().await else { return };
    let reclone = "mutation($id: ID!) { recloneRepository(repo: $id) { alwaysNil } }";
    env.ok(reclone, json!({ "id": env.gql_id(DEMO) })).await;
    let reason = || async {
        sqlx::query_as::<_, (String, i16)>(
            "SELECT reason, priority FROM repo_update_jobs WHERE repo_id = $1 AND state = 'queued'",
        )
        .bind(env.ids[DEMO])
        .fetch_one(env.db.pool())
        .await
        .unwrap()
    };
    assert_eq!(reason().await, ("reclone".to_owned(), 30));
    // A fetch requested meanwhile coalesces into the queued re-clone and keeps it one.
    env.ok(
        "mutation($id: ID!) { updateMirrorRepository(repository: $id) { alwaysNil } }",
        json!({ "id": env.gql_id(DEMO) }),
    )
    .await;
    assert_eq!(reason().await, ("reclone".to_owned(), 30));

    env.forbidden(reclone, json!({ "id": env.gql_id(DEMO) }))
        .await;
    let missing = env
        .run(
            env.admin,
            reclone,
            json!({ "id": marshal_id("Repository", 999_999) }),
        )
        .await;
    assert!(!missing.errors.is_empty());
}

#[tokio::test]
async fn gitservers_disk_usage() {
    let Some(env) = setup().await else { return };
    let q = "{ gitservers { totalCount nodes { id address freeDiskSpaceBytes totalDiskSpaceBytes
        configured error repositoriesCount assignedRepositoriesCount repositoriesDiskBytes
        repositoriesOnDisk indexedRepositoriesCount indexDiskBytes } } }";
    let data = env.ok(q, json!({})).await;
    let g = &data["gitservers"];
    assert_eq!(g["totalCount"], 2);
    let node = &g["nodes"][0];
    assert_eq!(node["address"], env.node.addr.as_str());
    assert_eq!(node["configured"], true);
    assert_eq!(node["error"], Value::Null);
    let total: u64 = node["totalDiskSpaceBytes"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let free: u64 = node["freeDiskSpaceBytes"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(total > 0 && free <= total);
    assert_eq!(node["repositoriesCount"], 2);
    assert_eq!(node["repositoriesDiskBytes"], "1500");
    assert_eq!(node["repositoriesOnDisk"], 1);
    assert_eq!(node["indexedRepositoriesCount"], 1);
    let index: u64 = node["indexDiskBytes"].as_str().unwrap().parse().unwrap();
    assert!(index > 0);

    let gone = &g["nodes"][1];
    assert_eq!(gone["address"], GONE);
    assert_eq!(gone["configured"], false);
    assert!(gone["error"].is_string());
    assert_eq!(gone["assignedRepositoriesCount"], 1);
    assert_eq!(gone["totalDiskSpaceBytes"], "0");
    assert_eq!(gone["repositoriesOnDisk"], Value::Null);

    env.forbidden("{ gitservers { totalCount } }", json!({}))
        .await;
}

#[tokio::test]
async fn repo_rebalance_status_and_retry() {
    let Some(env) = setup().await else { return };
    let moves = env.db.repo_moves();
    // The in-process node runs with itself as the only node; GONE runs with an older list.
    moves
        .heartbeat(
            &env.node.addr,
            std::slice::from_ref(&env.node.addr),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    moves
        .heartbeat(GONE, &[GONE.to_owned()], chrono::Utc::now())
        .await
        .unwrap();
    // UNCLONED moves from GONE to the node: queued, then claimed and failed for good.
    let mut planning = moves.begin_planning().await.unwrap().unwrap();
    planning
        .queue(&[sourcegull_db::NewRepoMove {
            repo_id: env.ids[UNCLONED],
            from_node: GONE.to_owned(),
            to_node: env.node.addr.clone(),
            kind: sourcegull_db::MoveKind::Copy,
        }])
        .await
        .unwrap();
    planning.commit().await.unwrap();
    let claimed = moves.claim(&env.node.addr, 10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].repo_name, UNCLONED);
    assert_eq!(claimed[0].attempts, 1);
    moves
        .fail(
            env.ids[UNCLONED],
            &env.node.addr,
            "connection refused",
            None,
        )
        .await
        .unwrap();

    let query = "{ repoRebalance {
        waitingFor queued copying flipped failed done cancelled
        nodes { address nodeList alive configured }
        moves(states: [\"failed\"]) { repositoryName repositoryId fromNode toNode kind state
            attempts bytesCopied lastError finishedAt }
    } }";
    let data = env.ok(query, json!({})).await;
    let r = &data["repoRebalance"];
    assert_eq!(r["waitingFor"], json!([]));
    assert_eq!(
        (r["failed"].as_i64(), r["queued"].as_i64()),
        (Some(1), Some(0))
    );
    let nodes = r["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    let gone = nodes.iter().find(|n| n["address"] == GONE).unwrap();
    assert_eq!(gone["configured"], json!(false));
    assert_eq!(gone["alive"], json!(true));
    let m = &r["moves"][0];
    assert_eq!(m["repositoryName"], UNCLONED);
    assert_eq!(m["repositoryId"], env.gql_id(UNCLONED));
    assert_eq!(m["fromNode"], GONE);
    assert_eq!(m["kind"], "copy");
    assert_eq!(m["lastError"], "connection refused");
    assert!(m["finishedAt"].is_string());

    let bad = env
        .run(
            env.admin,
            "{ repoRebalance { moves(states: [\"nope\"]) { state } } }",
            json!({}),
        )
        .await;
    assert!(!bad.errors.is_empty());

    let retry = "mutation { retryFailedRepoMoves }";
    assert_eq!(env.ok(retry, json!({})).await["retryFailedRepoMoves"], 1);
    let data = env.ok(query, json!({})).await;
    assert_eq!(data["repoRebalance"]["queued"], 1);
    assert_eq!(data["repoRebalance"]["failed"], 0);

    env.forbidden(query, json!({})).await;
    env.forbidden(retry, json!({})).await;
}

const STATUS: &str = "{ statusMessages {
    __typename
    ... on GitUpdatesDisabled { message }
    ... on NoRepositoriesDetected { message }
    ... on CloningProgress { message cloned cloning queued total }
    ... on ExternalServiceSyncError { message externalService { displayName } }
    ... on SyncError { message }
    ... on IndexingProgress { notIndexed indexed }
    ... on GitserverDiskThresholdReached { message address clonesPaused }
} }";

fn of_type<'a>(data: &'a Value, ty: &str) -> Vec<&'a Value> {
    data["statusMessages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["__typename"] == ty)
        .collect()
}

#[tokio::test]
async fn status_messages() {
    let Some(env) = setup().await else { return };
    let data = env.ok(STATUS, json!({})).await;
    // BROKEN failed; UNCLONED is queued; CORRUPT is cloned without an index.
    let sync = of_type(&data, "SyncError");
    assert_eq!(
        sync[0]["message"],
        "2 repositories failed last attempt to sync content from code host"
    );
    let cloning = of_type(&data, "CloningProgress");
    assert_eq!(cloning.len(), 1);
    assert_eq!(cloning[0]["message"], "1 repository enqueued for cloning.");
    assert_eq!(cloning[0]["cloned"], 2);
    assert_eq!(cloning[0]["queued"], 1);
    assert_eq!(cloning[0]["total"], 4);
    let indexing = of_type(&data, "IndexingProgress");
    assert_eq!(indexing[0]["notIndexed"], 1);
    assert_eq!(indexing[0]["indexed"], 1);
    assert!(of_type(&data, "ExternalServiceSyncError").is_empty());
    assert!(of_type(&data, "GitUpdatesDisabled").is_empty());
    assert!(of_type(&data, "NoRepositoriesDetected").is_empty());
    assert!(
        of_type(&data, "GitserverDiskThresholdReached")
            .iter()
            .all(|m| m["clonesPaused"] == false)
    );

    // The repo node paused clones for UNCLONED; a connection's last sync failed.
    env.db
        .gitserver_repos()
        .record_error(env.ids[UNCLONED], GONE, sourcegull_db::CLONES_PAUSED_ERROR)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO external_services (kind, display_name, config_enc, last_sync_error)
         VALUES ('GITLAB', 'GitLab', '\\x00', 'GitLab API: 401 Unauthorized')",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    let data = env.ok(STATUS, json!({})).await;
    let disk = of_type(&data, "GitserverDiskThresholdReached");
    let paused: Vec<_> = disk.iter().filter(|m| m["clonesPaused"] == true).collect();
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0]["address"], GONE);
    assert!(
        paused[0]["message"]
            .as_str()
            .unwrap()
            .contains("Clones are paused on repo node \"http://127.0.0.1:1\""),
        "{paused:?}"
    );
    // The paused repo is not counted again as failed or queued.
    assert_eq!(
        of_type(&data, "SyncError")[0]["message"],
        "2 repositories failed last attempt to sync content from code host"
    );
    assert!(of_type(&data, "CloningProgress").is_empty());
    let ext = of_type(&data, "ExternalServiceSyncError");
    assert_eq!(ext[0]["message"], "GitLab API: 401 Unauthorized");
    assert_eq!(ext[0]["externalService"]["displayName"], "GitLab");

    let stats = env
        .ok("{ repositoryStats { queued notIndexed } }", json!({}))
        .await;
    assert_eq!(stats["repositoryStats"]["queued"], 0);
    assert_eq!(stats["repositoryStats"]["notIndexed"], 1);

    // Site admins only.
    env.forbidden(STATUS, json!({})).await;
}

/// A GitLab project without a fetchable repository (none yet, the repository feature
/// disabled, or the token below Reporter) is an empty clone, not a failure: no last error,
/// not failed, queued or waiting for an index, and `mirrorInfo` says why.
async fn unavailable_repository_is_empty_not_failed(why: RemoteUnavailable) {
    let Some(env) = setup().await else { return };
    let gs = env.db.gitserver_repos();
    let id = env.ids[UNCLONED];
    // The first attempt failed before the repo node knew the message; the next one records
    // the empty clone and clears the error.
    gs.record_error(id, &env.node.addr, MISSING_LAST_ERROR)
        .await
        .unwrap();
    gs.set_cloning(id, &env.node.addr).await.unwrap();
    gs.record_fetched(
        id,
        &FetchResult {
            changed: false,
            head_oid: None,
            size_bytes: Some(10),
            // The repository is assigned to GONE; only its own node records fetches.
            node: GONE.to_owned(),
            remote_unavailable: Some(why),
        },
    )
    .await
    .unwrap();

    let data = env
        .ok(
            "query($name: String!) { repository(name: $name) { mirrorInfo {
                cloned cloneInProgress lastError isEmpty remoteMissing remoteUnavailable
                noCodeAccess } } }",
            json!({ "name": UNCLONED }),
        )
        .await;
    let m = &data["repository"]["mirrorInfo"];
    assert_eq!(m["cloned"], true);
    assert_eq!(m["lastError"], Value::Null);
    assert_eq!(m["isEmpty"], true);
    let no_access = why == RemoteUnavailable::NoCodeAccess;
    assert_eq!(m["remoteMissing"], !no_access);
    assert_eq!(m["noCodeAccess"], no_access);
    assert_eq!(m["remoteUnavailable"], why.as_str().to_ascii_uppercase());
    let demo = env
        .ok(
            "query($name: String!) { repository(name: $name) {
                mirrorInfo { isEmpty remoteMissing remoteUnavailable noCodeAccess } } }",
            json!({ "name": DEMO }),
        )
        .await;
    assert_eq!(demo["repository"]["mirrorInfo"]["isEmpty"], false);
    assert_eq!(demo["repository"]["mirrorInfo"]["remoteMissing"], false);
    assert_eq!(
        demo["repository"]["mirrorInfo"]["remoteUnavailable"],
        Value::Null
    );
    assert_eq!(demo["repository"]["mirrorInfo"]["noCodeAccess"], false);

    let stats = env
        .ok(
            "{ repositoryStats { cloned notCloned failedFetch queued notIndexed noCodeAccess } }",
            json!({}),
        )
        .await;
    let s = &stats["repositoryStats"];
    assert_eq!(s["cloned"], 3);
    assert_eq!(s["notCloned"], 1);
    assert_eq!(s["failedFetch"], 2); // BROKEN and CORRUPT only
    assert_eq!(s["queued"], 0);
    assert_eq!(s["notIndexed"], 1); // CORRUPT; the empty repo has nothing to index
    assert_eq!(s["noCodeAccess"], i32::from(no_access));
    let data = env.ok(STATUS, json!({})).await;
    assert_eq!(
        of_type(&data, "SyncError")[0]["message"],
        "2 repositories failed last attempt to sync content from code host"
    );
    assert!(of_type(&data, "CloningProgress").is_empty());
    assert_eq!(of_type(&data, "IndexingProgress")[0]["notIndexed"], 1);

    // The first push arrives: the next fetch clears the flag.
    gs.record_fetched(
        id,
        &FetchResult {
            changed: true,
            head_oid: Some("1".repeat(40)),
            size_bytes: Some(100),
            node: GONE.to_owned(),
            remote_unavailable: None,
        },
    )
    .await
    .unwrap();
    let row = gs.get(id).await.unwrap().unwrap();
    assert_eq!(row.remote_unavailable, None);
}

#[tokio::test]
async fn project_without_repository_is_empty_not_failed() {
    unavailable_repository_is_empty_not_failed(RemoteUnavailable::Missing).await;
}

#[tokio::test]
async fn disabled_repository_is_empty_not_failed() {
    unavailable_repository_is_empty_not_failed(RemoteUnavailable::Disabled).await;
}

#[tokio::test]
async fn no_code_access_is_a_warning_not_failed() {
    unavailable_repository_is_empty_not_failed(RemoteUnavailable::NoCodeAccess).await;
}
