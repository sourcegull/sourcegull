//! Code host connection admin GraphQL tests: connection checks, live config
//! validation, webhook secrets and the sync history, against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`) and the mock GitLab server.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use axum::http::HeaderMap;
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
use sourcegull_types::{ExternalServiceId, ExternalServiceKind, UserId};
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
    addExternalService(input: $input) { id } }";
const UPDATE: &str = "mutation Update($input: UpdateExternalServiceInput!) {
    updateExternalService(input: $input) { id warning } }";
const SERVICE: &str = "query S($id: ID!) { node(id: $id) { ... on ExternalService {
    url config unrestricted hasConnectionCheck supportsRepoExclusion webhookURL webhookSecret } } }";
const CHECK: &str = "query C($id: ID!) { node(id: $id) { ... on ExternalService {
    checkConnection {
        __typename
        ... on ExternalServiceAvailable { lastCheckedAt }
        ... on ExternalServiceUnavailable { suspectedReason } } } } }";
const JOBS: &str = "query J($id: ID!, $first: Int, $after: String) { node(id: $id) {
    ... on ExternalService { syncJobs(first: $first, after: $after) {
        totalCount pageInfo { hasNextPage endCursor }
        nodes { id state reason reposSynced reposAdded repoSyncErrors failureMessage
                startedAt finishedAt } } } } }";
const VALIDATE: &str = "query V($config: String!) {
    validateExternalServiceConfig(kind: GITLAB, config: $config) { errors warnings } }";
const CHECK_CONFIG: &str = "query K($config: String!, $id: ID) {
    checkExternalServiceConfig(kind: GITLAB, config: $config, id: $id) {
        ok problem account scopes warnings } }";
const CANCEL: &str = "mutation C($id: ID!) { cancelExternalServiceSync(id: $id) { alwaysNil } }";
const REGENERATE: &str = "mutation R($id: ID!) {
    regenerateExternalServiceWebhookSecret(id: $id) {
        webhookSecret externalService { id webhookSecret config } } }";
const SYNC: &str = "mutation S($id: ID!) { syncExternalService(id: $id) { alwaysNil } }";

impl Env {
    async fn add(&self) -> (String, ExternalServiceId) {
        let input = json!({ "input": { "kind": "GITLAB", "displayName": "GitLab",
            "config": self.config_text("") } });
        let id = self.ok(Some(self.admin), ADD, input).await["addExternalService"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let sid = ExternalServiceId(
            sourcegull_services::relay::unmarshal_int_id("ExternalService", &id).unwrap(),
        );
        (id, sid)
    }

    async fn jobs(&self, id: &str, first: i32, after: Option<&str>) -> Value {
        let data = self
            .ok(
                Some(self.admin),
                JOBS,
                json!({ "id": id, "first": first, "after": after }),
            )
            .await;
        data["node"]["syncJobs"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_only() {
    let Some(env) = setup().await else { return };
    let (id, _) = env.add().await;
    let config = json!({ "config": env.config_text("") });
    // Non-admins get an error for every operation; anonymous users must sign in.
    for (query, vars) in [
        (VALIDATE, config.clone()),
        (CHECK_CONFIG, config.clone()),
        (CANCEL, json!({ "id": "RXh0ZXJuYWxTZXJ2aWNlU3luY0pvYjox" })),
        (REGENERATE, json!({ "id": id })),
        (SYNC, json!({ "id": id })),
        (CHECK, json!({ "id": id })),
        (
            "mutation D($id: ID!) { deleteExternalService(externalService: $id) { alwaysNil } }",
            json!({ "id": id }),
        ),
    ] {
        let resp = env
            .err(Some(env.alice), query, vars.clone(), "FORBIDDEN")
            .await;
        assert_eq!(
            resp.errors[0].message, "Must be site admin to perform this action.",
            "{query}"
        );
        env.err(None, query, vars, "UNAUTHORIZED").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_and_inspect_connection() {
    let Some(env) = setup().await else { return };

    // Live validation lists schema errors and warnings without saving anything.
    let v = env
        .ok(
            Some(env.admin),
            VALIDATE,
            json!({ "config": r#"{"url": "https://gitlab.com", "token": "t", "nope": 1}"# }),
        )
        .await;
    let errors = v["validateExternalServiceConfig"]["errors"]
        .as_array()
        .unwrap();
    assert!(
        errors.iter().any(|e| e.as_str().unwrap().contains("nope")),
        "{errors:?}"
    );
    let v = env
        .ok(Some(env.admin), VALIDATE, json!({ "config": "{" }))
        .await;
    assert!(
        v["validateExternalServiceConfig"]["errors"][0]
            .as_str()
            .unwrap()
            .starts_with("syntax error")
    );
    let v = env
        .ok(
            Some(env.admin),
            VALIDATE,
            json!({ "config": env.config_text(r#", "token.oauth.refresh": "r""#) }),
        )
        .await;
    assert_eq!(v["validateExternalServiceConfig"]["errors"], json!([]));
    assert_eq!(
        v["validateExternalServiceConfig"]["warnings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let count: i64 = sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM external_services")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    let (id, sid) = env.add().await;
    let svc = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await["node"].clone();
    assert_eq!(
        svc["url"],
        format!("{}/", env.gitlab.url.trim_end_matches('/'))
    );
    assert_eq!(svc["unrestricted"], true);
    assert_eq!(svc["hasConnectionCheck"], true);
    assert_eq!(svc["supportsRepoExclusion"], true);
    // Short secrets are masked without a tail.
    assert_eq!(svc["webhookSecret"], "…");
    assert!(!svc["config"].as_str().unwrap().contains("hook-secret"));

    // Test connection: the mock accepts the token.
    let c = env.ok(Some(env.admin), CHECK, json!({ "id": id })).await;
    assert_eq!(
        c["node"]["checkConnection"]["__typename"],
        "ExternalServiceAvailable"
    );
    assert!(c["node"]["checkConnection"]["lastCheckedAt"].is_string());

    // A rejected token makes the connection unavailable, with the reason.
    let wrong = env
        .config_text("")
        .replace(r#""token": "tok""#, r#""token": "wrong""#);
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": wrong } }),
    )
    .await;
    let c = env.ok(Some(env.admin), CHECK, json!({ "id": id })).await;
    let check = &c["node"]["checkConnection"];
    assert_eq!(check["__typename"], "ExternalServiceUnavailable");
    assert!(
        check["suspectedReason"]
            .as_str()
            .unwrap()
            .contains("unauthorized"),
        "{check}"
    );

    // Editing the redacted config keeps the secrets and the comments.
    let redacted = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await["node"]["config"]
        .as_str()
        .unwrap()
        .to_owned();
    let edited = redacted.replace("// mock GitLab", "// mock GitLab, edited");
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": edited } }),
    )
    .await;
    let stored = env
        .db
        .external_services(Some(env.keyring.clone()))
        .config(sid)
        .await
        .unwrap();
    assert!(
        stored.contains("// mock GitLab, edited") && stored.contains("hook-secret"),
        "{stored}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_unsaved_config() {
    let Some(env) = setup().await else { return };
    let env = &env;
    let check = |vars: Value| async move {
        env.ok(Some(env.admin), CHECK_CONFIG, vars).await["checkExternalServiceConfig"].clone()
    };

    // An unsaved configuration is checked against the code host; nothing is stored.
    let c = check(json!({ "config": env.config_text("") })).await;
    assert_eq!(c["ok"], true, "{c}");
    assert_eq!(c["account"], "root");
    assert_eq!(c["problem"], Value::Null);
    let count: i64 = sourcegull_db::sqlx::query_scalar("SELECT count(*) FROM external_services")
        .fetch_one(env.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    // A rejected token and an invalid configuration fail with the reason.
    let wrong = env
        .config_text("")
        .replace(r#""token": "tok""#, r#""token": "wrong""#);
    let c = check(json!({ "config": wrong })).await;
    assert_eq!(c["ok"], false);
    assert!(
        c["problem"].as_str().unwrap().contains("unauthorized"),
        "{c}"
    );
    let c = check(json!({ "config": r#"{"url": "https://gitlab.com", "nope": 1}"# })).await;
    assert_eq!(c["ok"], false);
    assert!(
        c["problem"]
            .as_str()
            .unwrap()
            .starts_with("The configuration is invalid"),
        "{c}"
    );

    // With the connection's id, REDACTED secrets are the stored ones.
    let (id, _) = env.add().await;
    let redacted = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await["node"]["config"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(redacted.contains("REDACTED"), "{redacted}");
    let c = check(json!({ "config": redacted, "id": id })).await;
    assert_eq!(c["ok"], true, "{c}");
    let c = check(json!({ "config": redacted })).await;
    assert_eq!(c["ok"], false, "{c}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regenerate_webhook_secret() {
    let Some(env) = setup().await else { return };
    let (id, sid) = env.add().await;
    let before = env.jobs(&id, 10, None).await["totalCount"].clone();

    let data = env
        .ok(Some(env.admin), REGENERATE, json!({ "id": id }))
        .await;
    let payload = &data["regenerateExternalServiceWebhookSecret"];
    let secret = payload["webhookSecret"].as_str().unwrap().to_owned();
    assert_eq!(secret.len(), 43);
    assert_ne!(secret, "hook-secret");
    // The connection in the payload and every later read show it masked.
    let masked = format!("…{}", &secret[secret.len() - 4..]);
    let svc = &payload["externalService"];
    assert_eq!(svc["webhookSecret"], masked.as_str());
    let config = svc["config"].as_str().unwrap();
    assert!(config.contains("// mock GitLab") && !config.contains(&secret));
    let read = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await;
    assert_eq!(read["node"]["webhookSecret"], masked.as_str());
    assert!(!read.to_string().contains(&secret));

    // The stored config has only the new secret, and no sync was queued for it.
    let stored = env
        .db
        .external_services(Some(env.keyring.clone()))
        .config(sid)
        .await
        .unwrap();
    assert!(stored.contains(&secret) && !stored.contains("hook-secret"));
    assert_eq!(env.jobs(&id, 10, None).await["totalCount"], before);

    // The webhook receiver, reading the stored config, accepts the new secret only.
    let handler = Registry::default()
        .webhook_handler(ExternalServiceKind::GitLab, &stored)
        .unwrap();
    let headers = |token: &str| {
        let mut h = HeaderMap::new();
        h.insert("x-gitlab-token", token.parse().unwrap());
        h
    };
    assert!(handler.verify(&headers(&secret), b"{}").is_ok());
    assert!(handler.verify(&headers("hook-secret"), b"{}").is_err());

    // A connection without webhooks gets one.
    let none = env.config_text("").replace(
        r#""webhooks": [{"secret": "hook-secret"}] "#,
        r#""webhooks": [] "#,
    );
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": none } }),
    )
    .await;
    let svc = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await;
    assert_eq!(svc["node"]["webhookSecret"], Value::Null);
    let data = env
        .ok(Some(env.admin), REGENERATE, json!({ "id": id }))
        .await;
    let payload = &data["regenerateExternalServiceWebhookSecret"];
    let secret = payload["webhookSecret"].as_str().unwrap();
    assert_eq!(
        payload["externalService"]["webhookSecret"],
        format!("…{}", &secret[secret.len() - 4..])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redacted_save_keeps_refreshed_oauth_tokens() {
    let Some(env) = setup().await else { return };
    let oauth = r#", "token.type": "oauth", "token.oauth.refresh": "r1",
  "token.oauth.expiry": 4102444800"#;
    let input = json!({ "input": { "kind": "GITLAB", "displayName": "GitLab",
        "config": env.config_text(oauth) } });
    let id = env.ok(Some(env.admin), ADD, input).await["addExternalService"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sid = ExternalServiceId(
        sourcegull_services::relay::unmarshal_int_id("ExternalService", &id).unwrap(),
    );

    // The admin reads the redacted config...
    let redacted = env.ok(Some(env.admin), SERVICE, json!({ "id": id })).await["node"]["config"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!redacted.contains("r1"), "{redacted}");

    // ...and saves an edit with the token fields still REDACTED while a token refresh holds
    // the row lock and writes rotated tokens back. The save must see the rotated tokens.
    let store = env.db.external_services(Some(env.keyring.clone()));
    let locked = store.lock_config(sid).await.unwrap();
    let refreshed = sourcegull_codehost_gitlab::config::set_oauth_tokens(
        &locked.config,
        &sourcegull_codehost_gitlab::OAuthTokens {
            access_token: "tok2".into(),
            refresh_token: Some("r2".into()),
            expires_at: Some(4102448400),
        },
    )
    .unwrap();
    let edited = redacted.replace("// mock GitLab", "// mock GitLab, edited");
    let save = env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": edited } }),
    );
    let refresh = async {
        // Let the save reach the database before the refresh commits.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        locked.replace(&refreshed).await.unwrap();
    };
    tokio::join!(save, refresh);
    let stored = store.config(sid).await.unwrap();
    assert!(stored.contains("// mock GitLab, edited"), "{stored}");
    for kept in [
        r#""token": "tok2""#,
        r#""token.oauth.refresh": "r2""#,
        r#""token.oauth.expiry": 4102448400"#,
    ] {
        assert!(stored.contains(kept), "{kept} missing: {stored}");
    }

    // Token values the admin enters win.
    let entered = edited
        .replacen(r#""token": "REDACTED""#, r#""token": "tok""#, 1)
        .replacen(
            r#""token.oauth.refresh": "REDACTED""#,
            r#""token.oauth.refresh": "r3""#,
            1,
        );
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "input": { "id": id, "config": entered } }),
    )
    .await;
    let stored = store.config(sid).await.unwrap();
    assert!(
        stored.contains(r#""token": "tok""#) && stored.contains(r#""token.oauth.refresh": "r3""#),
        "{stored}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_history_and_cancel() {
    let Some(env) = setup().await else { return };
    let (id, sid) = env.add().await;

    // The first sync is queued on create; cancel it.
    let jobs = env.jobs(&id, 10, None).await;
    assert_eq!(jobs["totalCount"], 1);
    let first = jobs["nodes"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(jobs["nodes"][0]["state"], "QUEUED");
    env.ok(Some(env.admin), CANCEL, json!({ "id": first }))
        .await;
    let jobs = env.jobs(&id, 10, None).await;
    assert_eq!(jobs["nodes"][0]["state"], "CANCELED");
    env.err(
        Some(env.admin),
        CANCEL,
        json!({ "id": first }),
        "INVALID_INPUT",
    )
    .await;
    env.err(
        Some(env.admin),
        CANCEL,
        json!({ "id": "RXh0ZXJuYWxTZXJ2aWNlU3luY0pvYjo5OTk5OQ==" }),
        "NOT_FOUND",
    )
    .await;

    // Sync now, and run it as a worker would.
    env.ok(Some(env.admin), SYNC, json!({ "id": id })).await;
    let jobs = env.jobs(&id, 10, None).await;
    assert_eq!(jobs["totalCount"], 2);
    assert_eq!(jobs["nodes"][0]["reason"], "manual");
    let job = sourcegull_services::relay::unmarshal_int_id(
        "ExternalServiceSyncJob",
        jobs["nodes"][0]["id"].as_str().unwrap(),
    )
    .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE external_service_sync_jobs SET state = 'processing', started_at = now()
         WHERE id = $1",
    )
    .bind(job)
    .execute(env.db.pool())
    .await
    .unwrap();
    Syncer::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        Registry::default(),
        env.config.clone(),
    )
    .sync(sid, Some(job), &CancellationToken::new())
    .await
    .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE external_service_sync_jobs SET state = 'completed', finished_at = now()
         WHERE id = $1",
    )
    .bind(job)
    .execute(env.db.pool())
    .await
    .unwrap();

    // Pages of one, newest first.
    let page = env.jobs(&id, 1, None).await;
    assert_eq!(page["totalCount"], 2);
    assert_eq!(page["pageInfo"]["hasNextPage"], true);
    let done = &page["nodes"][0];
    assert_eq!(done["state"], "COMPLETED");
    assert_eq!(done["reposSynced"], 3);
    assert_eq!(done["reposAdded"], 3);
    assert_eq!(done["repoSyncErrors"], 0);
    let cursor = page["pageInfo"]["endCursor"].as_str().unwrap().to_owned();
    let older = env.jobs(&id, 1, Some(&cursor)).await;
    assert_eq!(older["nodes"][0]["id"], first);
    assert_eq!(older["pageInfo"]["hasNextPage"], false);

    // A running sync shows as canceling until its worker stops it.
    env.ok(Some(env.admin), SYNC, json!({ "id": id })).await;
    let running = env.jobs(&id, 1, None).await["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    sourcegull_db::sqlx::query(
        "UPDATE external_service_sync_jobs SET state = 'processing' WHERE state = 'queued'",
    )
    .execute(env.db.pool())
    .await
    .unwrap();
    env.ok(Some(env.admin), CANCEL, json!({ "id": running }))
        .await;
    assert_eq!(
        env.jobs(&id, 1, None).await["nodes"][0]["state"],
        "CANCELING"
    );
}

/// `rateLimiterState`: two "processes" with their own Postgres-backed limiters — the
/// web schema and a sync worker — see one budget and one pause per connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limiter_state_is_shared() {
    let Some(env) = setup().await else { return };
    let (id, sid) = env.add().await;
    let web = Registry {
        limiters: Some(Arc::new(sourcegull_ratelimit::PgRateLimiters::new(
            env.db.pool().clone(),
        ))),
        ..Registry::default()
    };
    let schema = build_schema(Services {
        registry: web,
        ..services(&env.db, &env.auth, &env.config, Some(env.keyring.clone()))
    });
    const STATE: &str = "query S($id: ID!) { node(id: $id) { ... on ExternalService {
        rateLimiterState { burst currentCapacity infinite interval lastReplenishment limit
                           pausedUntil } } } }";
    let state = |schema: &SourcegullSchema| {
        let (schema, id) = (schema.clone(), id.clone());
        let env = &env;
        async move {
            let resp = env
                .run_on(&schema, Some(env.admin), STATE, json!({ "id": id }))
                .await;
            assert!(resp.errors.is_empty(), "{:?}", resp.errors);
            resp.data.into_json().unwrap()["node"]["rateLimiterState"].clone()
        }
    };

    // Untouched: the default budget (36,000/h, a burst of one second's worth), full.
    let s = state(&schema).await;
    assert_eq!(s["limit"], 36_000);
    assert_eq!(s["interval"], 3600);
    assert_eq!(s["burst"], 10);
    assert_eq!(s["currentCapacity"], 10);
    assert_eq!(s["infinite"], false);
    assert_eq!(s["pausedUntil"], Value::Null);

    // A worker process syncs; GitLab answers the first request with 429 Retry-After: 2.
    env.gitlab.with(|s| {
        s.rate_limited = 1;
        s.rate_limit_retry_after = 2;
    });
    let worker = Registry {
        limiters: Some(Arc::new(sourcegull_ratelimit::PgRateLimiters::new(
            env.db.pool().clone(),
        ))),
        ..Registry::default()
    };
    let syncer = Syncer::new(
        env.db.clone(),
        Some(env.keyring.clone()),
        worker,
        env.config.clone(),
    );
    let sync = tokio::spawn(async move { syncer.sync(sid, None, &CancellationToken::new()).await });
    // The web process sees the pause the worker recorded.
    let start = std::time::Instant::now();
    loop {
        let s = state(&schema).await;
        if !s["pausedUntil"].is_null() {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "no pause: {s}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let stats = sync.await.unwrap().unwrap();
    assert!(start.elapsed() >= std::time::Duration::from_millis(1500));
    assert_eq!(stats.synced, 3);
    // ...and the tokens the sync spent.
    let s = state(&schema).await;
    assert!(s["currentCapacity"].as_i64().unwrap() < 10, "{s}");
    assert_eq!(s["pausedUntil"], Value::Null);

    // Disabling the limit shows an infinite limiter.
    let update = "mutation U($id: ID!, $config: String!) {
        updateExternalService(input: { id: $id, config: $config }) { id } }";
    env.ok(
        Some(env.admin),
        update,
        json!({ "id": id, "config": env.config_text(r#", "rateLimit": {"enabled": false, "requestsPerHour": 1}"#) }),
    )
    .await;
    let s = state(&schema).await;
    assert_eq!(s["infinite"], true);
    assert_eq!(s["limit"], 0);
}
