//! Code host connection and repository GraphQL tests against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and the mock GitLab server.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_config::{AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_repos::{Registry, Syncer};
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceId, UserId};
use tokio_util::sync::CancellationToken;

const PW: &str = "correct horse battery";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    keyring: Arc<Keyring>,
    config: ConfigHandle<SiteConfig>,
    schema: SourcegullSchema,
    gitlab: MockGitLab,
    admin: UserId,
    alice: UserId,
}

async fn setup() -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate");
    let db = Db::from_pool(tdb.pool.clone());
    let config = ConfigHandle::new(1, site_config(false));
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let admin = auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .unwrap();
    let alice = auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let provider = Arc::new(StaticKeyProvider::new("k1", [9u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let schema = build_schema(services(&db, &auth, &config, Some(keyring.clone())));
    let gitlab = MockGitLab::start("tok").await;
    let day = |d| Utc.with_ymd_and_hms(2026, 1, d, 0, 0, 0).unwrap();
    gitlab.with(|s| {
        s.projects = vec![
            project(1, "acme/api", day(1)),
            project(2, "acme/web", day(2)),
            project(3, "acme/docs", day(3)),
        ];
    });
    Some(Env {
        _t: tdb,
        db,
        auth,
        keyring,
        config,
        schema,
        gitlab,
        admin: admin.id,
        alice: alice.id,
    })
}

fn site_config(public: bool) -> SiteConfig {
    SiteConfig {
        external_url: Some("https://sourcegull.example.com/".into()),
        auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider {
            allow_signup: true,
        })],
        auth_public: public,
        ..SiteConfig::default()
    }
}

fn services(
    db: &Db,
    auth: &Authenticator,
    config: &ConfigHandle<SiteConfig>,
    keyring: Option<Arc<Keyring>>,
) -> Services {
    Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring,
        registry: Registry::default(),
        repo_clients: std::sync::Arc::new(sourcegull_rpc::RepoClients::new(
            sourcegull_rpc::Router::default(),
            sourcegull_rpc::ClientConfig::default(),
        )),
        authz: sourcegull_authz::Checker::new(db.clone(), config.clone()),
        app: None,
    }
}

impl Env {
    async fn run_on(
        &self,
        schema: &SourcegullSchema,
        actor: Option<UserId>,
        query: &str,
        vars: Value,
    ) -> Response {
        let actor = match actor {
            Some(id) => self.auth.actor_for(id, AuthVia::Session).await.unwrap(),
            None => None,
        };
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(schema, req, actor).await
    }

    async fn ok(&self, actor: Option<UserId>, query: &str, vars: Value) -> Value {
        let resp = self.run_on(&self.schema, actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn err(&self, actor: Option<UserId>, query: &str, vars: Value, code: &str) -> Response {
        let resp = self.run_on(&self.schema, actor, query, vars).await;
        assert_eq!(
            error_code(&resp).as_deref(),
            Some(code),
            "{query}: {:?}",
            resp.errors
        );
        resp
    }

    fn config_text(&self, extra: &str) -> String {
        format!(
            r#"{{
  // mock GitLab
  "url": "{}",
  "token": "tok",
  "projectQuery": ["projects?membership=true"],
  "webhooks": [{{"secret": "hook-secret"}}] {extra}
}}"#,
            self.gitlab.url
        )
    }
}

fn error_code(resp: &Response) -> Option<String> {
    let v = resp
        .errors
        .first()?
        .extensions
        .as_ref()?
        .get("code")?
        .clone();
    v.into_json().ok()?.as_str().map(str::to_owned)
}

const ADD: &str = "mutation Add($input: AddExternalServiceInput!) {
    addExternalService(input: $input) { id kind displayName config warning webhookURL repoCount
        syncJobs { totalCount nodes { state reason } } } }";
const UPDATE: &str = "mutation Update($input: UpdateExternalServiceInput!) {
    updateExternalService(input: $input) { id displayName config warning } }";
const LIST: &str = "{ externalServices { totalCount pageInfo { hasNextPage }
    nodes { id displayName lastSyncAt lastSyncError } } }";
const REPOS: &str =
    "query Repos($first: Int, $after: String, $query: String, $cloneStatus: CloneStatus) {
    repositories(first: $first, after: $after, query: $query, cloneStatus: $cloneStatus) {
        totalCount pageInfo { hasNextPage endCursor } nodes { name } } }";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_service_lifecycle() {
    let Some(env) = setup().await else { return };

    // Site admins only.
    env.err(Some(env.alice), LIST, json!({}), "FORBIDDEN").await;
    let input = json!({ "input": { "kind": "GITLAB", "displayName": "GitLab", "config": env.config_text("") } });
    env.err(Some(env.alice), ADD, input.clone(), "FORBIDDEN")
        .await;

    // Invalid configs are rejected with the problems listed.
    let bad = json!({ "input": { "kind": "GITLAB", "displayName": "GitLab",
        "config": r#"{"url": "https://gitlab.com", "token": "t", "projectQuery": ["projects"], "nope": 1}"# } });
    let resp = env.err(Some(env.admin), ADD, bad, "VALIDATION").await;
    assert!(
        resp.errors[0].message.contains("nope"),
        "{}",
        resp.errors[0].message
    );

    // Without encryption keys nothing can be stored.
    let keyless = build_schema(services(&env.db, &env.auth, &env.config, None));
    let resp = env
        .run_on(&keyless, Some(env.admin), ADD, input.clone())
        .await;
    assert_eq!(error_code(&resp).as_deref(), Some("INTERNAL"));

    let data = env.ok(Some(env.admin), ADD, input).await;
    let svc = &data["addExternalService"];
    let id = svc["id"].as_str().unwrap().to_owned();
    assert_eq!(svc["kind"], "GITLAB");
    assert_eq!(svc["warning"], Value::Null, "mock token has read_api");
    let config = svc["config"].as_str().unwrap();
    assert!(config.contains("// mock GitLab"), "comments kept: {config}");
    assert!(!config.contains("\"tok\"") && !config.contains("hook-secret"));
    assert_eq!(config.matches("REDACTED").count(), 2);
    let webhook = svc["webhookURL"].as_str().unwrap();
    assert!(
        webhook.starts_with("https://sourcegull.example.com/.api/webhooks/"),
        "{webhook}"
    );
    assert_eq!(svc["syncJobs"]["nodes"][0]["state"], "QUEUED");
    assert_eq!(svc["syncJobs"]["nodes"][0]["reason"], "config_changed");

    // Update with the redacted config: secrets kept, no new sync needed.
    let redacted = config.to_owned();
    let data = env
        .ok(
            Some(env.admin),
            UPDATE,
            json!({ "input": { "id": id, "displayName": "Corp GitLab", "config": redacted } }),
        )
        .await;
    assert_eq!(data["updateExternalService"]["displayName"], "Corp GitLab");
    let sid = ExternalServiceId(
        sourcegull_services::relay::unmarshal_int_id("ExternalService", &id).unwrap(),
    );
    let stored = env
        .db
        .external_services(Some(env.keyring.clone()))
        .config(sid)
        .await
        .unwrap();
    assert!(stored.contains("\"tok\"") && stored.contains("hook-secret"));

    // A token the code host rejects is saved but warned about.
    let rotated = redacted.replace(r#""token": "REDACTED""#, r#""token": "wrong""#);
    let data = env
        .ok(
            Some(env.admin),
            UPDATE,
            json!({ "input": { "id": id, "config": rotated } }),
        )
        .await;
    let warning = data["updateExternalService"]["warning"].as_str().unwrap();
    assert!(warning.contains("unauthorized"), "{warning}");
    env.err(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": "{" } }),
        "VALIDATION",
    )
    .await;
    // Restore the working token.
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": env.config_text("") } }),
    )
    .await;

    env.ok(
        Some(env.admin),
        "mutation S($id: ID!) { syncExternalService(id: $id) { alwaysNil } }",
        json!({ "id": id }),
    )
    .await;
    let list = env.ok(Some(env.admin), LIST, json!({})).await;
    assert_eq!(list["externalServices"]["totalCount"], 1);
    assert_eq!(
        list["externalServices"]["nodes"][0]["displayName"],
        "Corp GitLab"
    );
    let node = env
        .ok(
            Some(env.admin),
            "query N($id: ID!) { node(id: $id) { ... on ExternalService { displayName } } }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(node["node"]["displayName"], "Corp GitLab");

    env.ok(
        Some(env.admin),
        "mutation D($id: ID!) { deleteExternalService(externalService: $id) { alwaysNil } }",
        json!({ "id": id }),
    )
    .await;
    let list = env.ok(Some(env.admin), LIST, json!({})).await;
    assert_eq!(list["externalServices"]["totalCount"], 0);
    let events: i64 = sourcegull_db::sqlx::query_scalar(
        "SELECT count(*) FROM security_event_logs WHERE name LIKE 'ExternalService%'",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(events, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repositories_queries() {
    let Some(env) = setup().await else { return };
    let input = json!({ "input": { "kind": "GITLAB", "displayName": "GitLab", "config": env.config_text("") } });
    let id = env.ok(Some(env.admin), ADD, input).await["addExternalService"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sid = ExternalServiceId(
        sourcegull_services::relay::unmarshal_int_id("ExternalService", &id).unwrap(),
    );
    Syncer::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        Registry::default(),
        env.config.clone(),
    )
    .sync(sid, None, &CancellationToken::new())
    .await
    .unwrap();
    let host = env.gitlab.url.trim_start_matches("http://").to_owned();

    env.err(None, REPOS, json!({}), "UNAUTHORIZED").await;
    let page = env.ok(Some(env.alice), REPOS, json!({ "first": 2 })).await;
    let repos = &page["repositories"];
    assert_eq!(repos["totalCount"], 3);
    assert_eq!(repos["pageInfo"]["hasNextPage"], true);
    assert_eq!(repos["nodes"][0]["name"], format!("{host}/acme/api"));
    let next = env
        .ok(
            Some(env.alice),
            REPOS,
            json!({ "first": 2, "after": repos["pageInfo"]["endCursor"] }),
        )
        .await;
    assert_eq!(
        next["repositories"]["nodes"][0]["name"],
        format!("{host}/acme/web")
    );
    assert_eq!(next["repositories"]["pageInfo"]["hasNextPage"], false);
    let filtered = env
        .ok(
            Some(env.alice),
            REPOS,
            json!({ "query": "DOC", "cloneStatus": "NOT_CLONED" }),
        )
        .await;
    assert_eq!(filtered["repositories"]["totalCount"], 1);
    let cloned = env
        .ok(Some(env.alice), REPOS, json!({ "cloneStatus": "CLONED" }))
        .await;
    assert_eq!(cloned["repositories"]["totalCount"], 0);

    const REPO: &str = "query R($name: String!) { repository(name: $name) {
        id name uri url description isFork isArchived isPrivate stars
        externalURLs { url serviceKind serviceType }
        externalRepository { id serviceType serviceID }
        mirrorInfo { cloned cloneInProgress lastError byteSize updatedAt } } }";
    let name = format!("{host}/acme/web");
    let data = env
        .ok(
            Some(env.alice),
            REPO,
            json!({ "name": name.to_uppercase() }),
        )
        .await;
    let repo = &data["repository"];
    assert_eq!(repo["name"], name);
    assert_eq!(repo["url"], format!("/{name}"));
    assert_eq!(repo["isPrivate"], true);
    assert_eq!(repo["stars"], 2);
    assert_eq!(repo["externalURLs"][0]["serviceKind"], "GITLAB");
    assert_eq!(repo["externalRepository"]["id"], "2");
    assert_eq!(
        repo["externalRepository"]["serviceID"],
        format!("{}/", env.gitlab.url)
    );
    assert_eq!(repo["mirrorInfo"]["cloned"], false);
    assert_eq!(repo["mirrorInfo"]["byteSize"], "0");
    let missing = env
        .ok(Some(env.alice), REPO, json!({ "name": "nope" }))
        .await;
    assert_eq!(missing["repository"], Value::Null);

    // Admin-only fields.
    const ADMIN: &str = "query R($name: String!) { repository(name: $name) {
        mirrorInfo { remoteURL } externalServices { totalCount } } }";
    env.err(Some(env.alice), ADMIN, json!({ "name": name }), "FORBIDDEN")
        .await;
    let data = env
        .ok(Some(env.admin), ADMIN, json!({ "name": name }))
        .await;
    assert_eq!(
        data["repository"]["mirrorInfo"]["remoteURL"],
        "https://gitlab.example.com/acme/web.git"
    );
    assert_eq!(data["repository"]["externalServices"]["totalCount"], 1);

    // node(id) and updateMirrorRepository.
    let repo_id = repo["id"].as_str().unwrap();
    let node = env
        .ok(
            Some(env.alice),
            "query N($id: ID!) { node(id: $id) { ... on Repository { name } } }",
            json!({ "id": repo_id }),
        )
        .await;
    assert_eq!(node["node"]["name"], name);
    const UPDATE_MIRROR: &str =
        "mutation U($id: ID!) { updateMirrorRepository(repository: $id) { alwaysNil } }";
    env.err(
        Some(env.alice),
        UPDATE_MIRROR,
        json!({ "id": repo_id }),
        "FORBIDDEN",
    )
    .await;
    env.ok(Some(env.admin), UPDATE_MIRROR, json!({ "id": repo_id }))
        .await;
    let priority: i16 = sourcegull_db::sqlx::query_scalar(
        "SELECT priority FROM repo_update_jobs WHERE state = 'queued' AND reason = 'manual'",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(priority, 30);

    // Public instances let anonymous users read repositories.
    env.config.store(2, site_config(true));
    let anon = env.ok(None, REPOS, json!({})).await;
    assert_eq!(anon["repositories"]["totalCount"], 3);
}
