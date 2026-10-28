//! Schema tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`), executing operations
//! as an injected actor, plus HTTP-level checks of `/.api/graphql`.

use std::path::PathBuf;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use axum::body::Body;
use axum::http::{self, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{
    Actor, AuthVia, Authenticator, MemorySessionStore, PasswordParams, Permission,
};
use sourcegull_config::{AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig};
use sourcegull_config::{SmtpAuthentication, SmtpConfig, SmtpEncryption};
use sourcegull_db::{Db, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_services::relay::marshal_id;
use sourcegull_services::{AppState, SiteConfigSource};
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;
use tower::ServiceExt;

const PW: &str = "correct horse battery";

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
}

impl Env {
    async fn actor(&self, id: UserId) -> Actor {
        self.auth
            .actor_for(id, AuthVia::Session)
            .await
            .expect("actor")
            .expect("user exists")
    }

    async fn run(&self, actor: Option<UserId>, query: &str, vars: Value) -> Response {
        let actor = match actor {
            Some(id) => Some(self.actor(id).await),
            None => None,
        };
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, actor).await
    }

    /// Runs and expects success; returns `data` as JSON.
    async fn ok(&self, actor: Option<UserId>, query: &str, vars: Value) -> Value {
        let resp = self.run(actor, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().expect("json")
    }

    /// Runs and expects an error with `extensions.code == code`.
    async fn err(&self, actor: Option<UserId>, query: &str, vars: Value, code: &str) -> Response {
        let resp = self.run(actor, query, vars).await;
        assert_eq!(
            error_code(&resp).as_deref(),
            Some(code),
            "{query}: {:?}",
            resp.errors
        );
        resp
    }
}

fn error_code(resp: &Response) -> Option<String> {
    let err = resp.errors.first()?;
    let v = err
        .extensions
        .as_ref()?
        .get("code")?
        .clone()
        .into_json()
        .ok()?;
    v.as_str().map(str::to_owned)
}

fn services(env_db: &Db, auth: &Authenticator, config: &ConfigHandle<SiteConfig>) -> Services {
    Services {
        db: env_db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: std::sync::Arc::new(sourcegull_rpc::RepoClients::new(
            sourcegull_rpc::Router::default(),
            sourcegull_rpc::ClientConfig::default(),
        )),
        authz: sourcegull_authz::Checker::new(env_db.clone(), config.clone()),
        app: None,
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider {
                allow_signup: true,
            })],
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    db.site_config()
        .create(sourcegull_config::default_site_config_jsonc(), None, None)
        .await
        .expect("default site config");
    let admin = auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .expect("admin");
    let alice = auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .expect("alice");
    assert!(admin.site_admin && !alice.site_admin);
    let schema = build_schema(services(&db, &auth, &config));
    Some(Env {
        _t: t,
        db,
        auth,
        config,
        schema,
        admin: admin.id,
        alice: alice.id,
    })
}

const SITE_CONFIG: &str = "query SiteConfiguration { site { id configuration { id effectiveContents validationMessages } } }";
const UPDATE: &str = "mutation UpdateSiteConfiguration($lastID: Int!, $input: String!) {
    updateSiteConfiguration(lastID: $lastID, input: $input) }";

#[tokio::test]
async fn rbac_admin_protection_and_immediate_permissions() {
    let Some(env) = setup().await else { return };
    let query =
        "query { roles { nodes { id name system } } permissions { nodes { id displayName } } }";
    env.err(Some(env.alice), query, json!({}), "FORBIDDEN")
        .await;
    env.err(
        Some(env.alice),
        "mutation { createRole(name: \"NOPE\", permissions: []) { id } }",
        json!({}),
        "FORBIDDEN",
    )
    .await;
    let data = env.ok(Some(env.admin), query, json!({})).await;
    let roles = data["roles"]["nodes"].as_array().unwrap();
    let user_role = roles.iter().find(|r| r["name"] == "USER").unwrap()["id"]
        .as_str()
        .unwrap();
    let admin_role = roles
        .iter()
        .find(|r| r["name"] == "SITE_ADMINISTRATOR")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    env.err(
        Some(env.admin),
        "mutation($role: ID!) { deleteRole(role: $role) { alwaysNil } }",
        json!({"role": user_role}),
        "FORBIDDEN",
    )
    .await;

    let ownership = data["permissions"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["displayName"] == "OWNERSHIP#ASSIGN")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let created = env.ok(Some(env.admin),
        "mutation($permissions: [ID!]!) { createRole(name: \"OWNERS\", permissions: $permissions) { id } }",
        json!({"permissions": [ownership]})).await;
    let role = created["createRole"]["id"].as_str().unwrap();
    let current = "query { currentUser { permissions { nodes { displayName } } } }";
    let before = env.ok(Some(env.alice), current, json!({})).await;
    assert!(
        !before["currentUser"]["permissions"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["displayName"] == "OWNERSHIP#ASSIGN")
    );
    let alice = marshal_id("User", env.alice.get());
    env.ok(Some(env.admin), "mutation($user: ID!, $roles: [ID!]!) { setRoles(user: $user, roles: $roles) { alwaysNil } }",
        json!({"user": alice, "roles": [role]})).await;
    let after = env.ok(Some(env.alice), current, json!({})).await;
    assert!(
        env.actor(env.alice)
            .await
            .permissions
            .has(Permission::OwnershipAssign)
    );
    assert!(
        after["currentUser"]["permissions"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["displayName"] == "OWNERSHIP#ASSIGN")
    );
    env.ok(Some(env.admin), "mutation($role: ID!, $permissions: [ID!]!) { setPermissions(role: $role, permissions: $permissions) { alwaysNil } }",
        json!({"role": role, "permissions": []})).await;
    let removed = env.ok(Some(env.alice), current, json!({})).await;
    assert!(
        !env.actor(env.alice)
            .await
            .permissions
            .has(Permission::OwnershipAssign)
    );
    assert!(
        !removed["currentUser"]["permissions"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["displayName"] == "OWNERSHIP#ASSIGN")
    );
    env.ok(Some(env.admin), "mutation($user: ID!, $roles: [ID!]!) { setRoles(user: $user, roles: $roles) { alwaysNil } }",
        json!({"user": alice, "roles": [admin_role]})).await;
    assert!(env.actor(env.alice).await.site_admin);
    assert!(
        env.db
            .users()
            .get_by_id(env.alice)
            .await
            .unwrap()
            .unwrap()
            .site_admin
    );
}

#[tokio::test]
async fn current_user_and_admin_only_fields() {
    let Some(env) = setup().await else { return };
    let q =
        "{ currentUser { id username displayName siteAdmin emails { email verified isPrimary } } }";
    let anon = env.ok(None, q, json!({})).await;
    assert_eq!(anon["currentUser"], Value::Null);

    let me = env.ok(Some(env.alice), q, json!({})).await;
    assert_eq!(me["currentUser"]["id"], marshal_id("User", env.alice.get()));
    assert_eq!(me["currentUser"]["username"], "alice");
    assert_eq!(me["currentUser"]["siteAdmin"], false);
    assert_eq!(me["currentUser"]["emails"][0]["email"], "alice@example.com");

    // site.configuration and users are site-admin only.
    env.err(None, SITE_CONFIG, json!({}), "UNAUTHORIZED").await;
    env.err(Some(env.alice), SITE_CONFIG, json!({}), "FORBIDDEN")
        .await;
    let users = "query Users($first: Int, $query: String) { users(first: $first, query: $query) {
        nodes { id username displayName siteAdmin createdAt } totalCount } }";
    env.err(Some(env.alice), users, json!({}), "FORBIDDEN")
        .await;
    let all = env.ok(Some(env.admin), users, json!({ "first": 1 })).await;
    assert_eq!(all["users"]["totalCount"], 2);
    assert_eq!(all["users"]["nodes"].as_array().map(Vec::len), Some(1));
    let filtered = env
        .ok(Some(env.admin), users, json!({ "query": "ali" }))
        .await;
    assert_eq!(filtered["users"]["totalCount"], 1);
    assert_eq!(filtered["users"]["nodes"][0]["username"], "alice");

    // Other users' emails are private to them and site admins.
    let by_name = "{ user(username: \"admin\") { username emails { email } } }";
    env.err(Some(env.alice), by_name, json!({}), "FORBIDDEN")
        .await;
    let d = env
        .ok(
            Some(env.admin),
            "{ user(username: \"alice\") { emails { email } } }",
            json!({}),
        )
        .await;
    assert_eq!(d["user"]["emails"][0]["email"], "alice@example.com");
}

#[tokio::test]
async fn site_configuration_update_conflict_and_validation() {
    let Some(env) = setup().await else { return };
    let d = env.ok(Some(env.admin), SITE_CONFIG, json!({})).await;
    let cfg = &d["site"]["configuration"];
    let id = cfg["id"].as_i64().expect("Int id");
    assert!(
        cfg["effectiveContents"]
            .as_str()
            .unwrap()
            .contains("auth.providers")
    );
    assert_eq!(cfg["validationMessages"], json!([]));
    assert!(d["site"]["id"].is_string());

    // Unknown keys are accepted and reported as validation messages.
    let input = "{\n  // comment kept\n  \"auth.providers\": [{\"type\": \"builtin\", \"allowSignup\": true}],\n  \"bogus\": 1,\n}";
    let r = env
        .ok(
            Some(env.admin),
            UPDATE,
            json!({ "lastID": id, "input": input }),
        )
        .await;
    assert_eq!(r["updateSiteConfiguration"], false);
    let d = env.ok(Some(env.admin), SITE_CONFIG, json!({})).await;
    let cfg = &d["site"]["configuration"];
    assert_eq!(cfg["id"].as_i64(), Some(id + 1));
    assert_eq!(cfg["effectiveContents"], input);
    let msgs = cfg["validationMessages"].as_array().expect("messages");
    assert!(
        msgs.iter().any(|m| m.as_str().unwrap().contains("bogus")),
        "{msgs:?}"
    );
    let latest = env.db.site_config().latest().await.unwrap().unwrap();
    assert_eq!(latest.author_user_id, Some(env.admin));

    // A stale lastID conflicts.
    env.err(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id, "input": "{}" }),
        "CONFLICT",
    )
    .await;

    // Invalid documents are rejected with the messages.
    let bad = r#"{"auth.public": "yes"}"#;
    let resp = env
        .err(
            Some(env.admin),
            UPDATE,
            json!({ "lastID": id + 1, "input": bad }),
            "VALIDATION",
        )
        .await;
    let ext = resp.errors[0].extensions.as_ref().unwrap();
    let messages = ext.get("messages").unwrap().clone().into_json().unwrap();
    assert!(!messages.as_array().unwrap().is_empty());
    env.err(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id + 1, "input": "{ nope" }),
        "VALIDATION",
    )
    .await;
    assert_eq!(
        env.db
            .site_config()
            .latest()
            .await
            .unwrap()
            .unwrap()
            .id
            .get(),
        id + 1
    );

    // Non-admins cannot write.
    env.err(
        Some(env.alice),
        UPDATE,
        json!({ "lastID": id + 1, "input": "{}" }),
        "FORBIDDEN",
    )
    .await;

    // The write was audited.
    let n: i64 = sourcegull_db::sqlx::query_scalar(
        "SELECT count(*) FROM security_event_logs WHERE name = 'SiteConfigUpdated'",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn site_configuration_secrets_are_redacted() {
    let Some(env) = setup().await else { return };
    let secrets = [
        "smtp-password-1",
        "executor-shared-secret-0123456789",
        "scim-token-0123456789",
        "gitlab-client-secret-1",
    ];
    let input = format!(
        r#"{{
  "externalURL": "https://sourcegull.example.com",
  // outgoing mail
  "email.address": "sourcegull@example.com",
  "email.smtp": {{"host": "smtp.example.com", "port": 587, "username": "u", "password": "{}", "authentication": "PLAIN"}},
  "executors.accessToken": "{}",
  "scim.authToken": "{}",
  "auth.providers": [
    {{"type": "builtin", "allowSignup": true}},
    {{"type": "gitlab", "url": "https://gitlab.com", "clientID": "app", "clientSecret": "{}"}},
  ],
}}"#,
        secrets[0], secrets[1], secrets[2], secrets[3]
    );
    async fn read(env: &Env) -> (i64, String, Value) {
        let d = env.ok(Some(env.admin), SITE_CONFIG, json!({})).await;
        let cfg = &d["site"]["configuration"];
        (
            cfg["id"].as_i64().unwrap(),
            cfg["effectiveContents"].as_str().unwrap().to_owned(),
            cfg["validationMessages"].clone(),
        )
    }
    async fn stored(env: &Env) -> String {
        env.db
            .site_config()
            .latest()
            .await
            .unwrap()
            .unwrap()
            .contents
    }
    let (id, _, _) = read(&env).await;
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id, "input": input }),
    )
    .await;
    assert_eq!(stored(&env).await, input);

    // Reads never contain the secrets; comments and the rest are kept, and the stored
    // document is what gets validated.
    let (id, redacted, messages) = read(&env).await;
    for secret in secrets {
        assert!(!redacted.contains(secret), "{redacted}");
    }
    assert_eq!(redacted.matches("\"REDACTED\"").count(), 4, "{redacted}");
    assert!(redacted.contains("// outgoing mail"));
    assert_eq!(messages, json!([]));

    // Saving the redacted document back keeps every secret.
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id, "input": redacted }),
    )
    .await;
    assert_eq!(stored(&env).await, input);

    // A new value replaces one secret; the others are kept.
    let (id, redacted, _) = read(&env).await;
    let rotated = redacted.replacen(
        r#""scim.authToken": "REDACTED""#,
        r#""scim.authToken": "${env:SOURCEGULL_SCIM_AUTH_TOKEN}""#,
        1,
    );
    env.ok(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id, "input": rotated }),
    )
    .await;
    assert_eq!(
        stored(&env).await,
        input.replace(secrets[2], "${env:SOURCEGULL_SCIM_AUTH_TOKEN}")
    );
    // References are not secrets and are shown as they are.
    let (id, redacted, _) = read(&env).await;
    assert!(
        redacted.contains("${env:SOURCEGULL_SCIM_AUTH_TOKEN}"),
        "{redacted}"
    );

    // The placeholder for a secret with no stored value is a validation error.
    let added = redacted.replacen(r#""clientID": "app""#, r#""clientID": "other-app""#, 1);
    let resp = env
        .err(
            Some(env.admin),
            UPDATE,
            json!({ "lastID": id, "input": added }),
            "VALIDATION",
        )
        .await;
    assert!(
        resp.errors[0]
            .message
            .contains("/auth.providers/1/clientSecret"),
        "{:?}",
        resp.errors
    );
    // A stale lastID still conflicts.
    env.err(
        Some(env.admin),
        UPDATE,
        json!({ "lastID": id - 1, "input": redacted }),
        "CONFLICT",
    )
    .await;
}

#[tokio::test]
async fn organize_site_configuration() {
    let Some(env) = setup().await else { return };
    let q = "query Organize($input: String!) { site { organizeConfiguration(input: $input) } }";
    let input = "{\n  \"log\": {},\n  // sign-in\n  \"auth.public\": true,\n}";
    let d = env.ok(Some(env.admin), q, json!({ "input": input })).await;
    assert_eq!(
        d["site"]["organizeConfiguration"],
        "{\n  // ── Authentication & sign-in ──\n  // sign-in\n  \"auth.public\": true,\n\n  // ── Observability & logs ──\n  \"log\": {},\n}\n"
    );
    env.err(Some(env.admin), q, json!({ "input": "{" }), "VALIDATION")
        .await;
    env.err(Some(env.alice), q, json!({ "input": "{}" }), "FORBIDDEN")
        .await;
    // Anyone can read the commented settings template.
    let d = env
        .ok(Some(env.alice), "{ site { defaultSettings } }", json!({}))
        .await;
    assert!(
        d["site"]["defaultSettings"]
            .as_str()
            .unwrap()
            .contains("// \"search.defaultPatternType\": \"keyword\",")
    );
}

#[tokio::test]
async fn read_only_site_configuration() {
    let Some(env) = setup().await else { return };
    let dir = std::env::temp_dir().join(format!("sourcegull-gql-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("site.jsonc");
    std::fs::write(&path, "{ \"auth.public\": false }").unwrap();
    let mut svc = services(&env.db, &env.auth, &env.config);
    svc.site_config_source = SiteConfigSource::File(path.clone());
    let schema = build_schema(svc);
    let actor = env.actor(env.admin).await;
    let resp =
        sourcegull_graphql::execute(&schema, Request::new(SITE_CONFIG), Some(actor.clone())).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(
        data["site"]["configuration"]["effectiveContents"],
        "{ \"auth.public\": false }"
    );
    let req =
        Request::new(UPDATE).variables(Variables::from_json(json!({ "lastID": 1, "input": "{}" })));
    let resp = sourcegull_graphql::execute(&schema, req, Some(actor)).await;
    assert_eq!(error_code(&resp).as_deref(), Some("READ_ONLY"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn access_tokens_create_list_delete() {
    let Some(env) = setup().await else { return };
    let create = "mutation CreateAccessToken($user: ID!, $scopes: [String!]!, $note: String!) {
        createAccessToken(user: $user, scopes: $scopes, note: $note) { id token } }";
    let list = "{ currentUser { accessTokens { nodes { id note scopes createdAt lastUsedAt expiresAt } totalCount } } }";
    let delete =
        "mutation DeleteAccessToken($byID: ID!) { deleteAccessToken(byID: $byID) { alwaysNil } }";
    let alice_id = marshal_id("User", env.alice.get());
    let admin_id = marshal_id("User", env.admin.get());

    let d = env
        .ok(
            Some(env.alice),
            create,
            json!({ "user": alice_id, "scopes": ["user:all"], "note": "ci" }),
        )
        .await;
    let token_id = d["createAccessToken"]["id"].as_str().unwrap().to_owned();
    let secret = d["createAccessToken"]["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("skp_"));

    // The token authenticates as alice.
    let actor = env
        .auth
        .authenticate_access_token(&format!("token {secret}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actor.user_id, env.alice);

    let d = env.ok(Some(env.alice), list, json!({})).await;
    let nodes = &d["currentUser"]["accessTokens"]["nodes"];
    assert_eq!(nodes.as_array().map(Vec::len), Some(1));
    assert_eq!(nodes[0]["id"], token_id);
    assert_eq!(nodes[0]["note"], "ci");
    assert_eq!(nodes[0]["scopes"], json!(["user:all"]));
    assert!(nodes[0]["expiresAt"].is_string());

    // Users cannot create tokens for, or list the tokens of, others; nor use sudo scope.
    env.err(
        Some(env.alice),
        create,
        json!({ "user": admin_id, "scopes": ["user:all"], "note": "x" }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        create,
        json!({ "user": alice_id, "scopes": ["user:all", "site-admin:sudo"], "note": "x" }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        "{ user(username: \"admin\") { accessTokens { totalCount } } }",
        json!({}),
        "FORBIDDEN",
    )
    .await;
    let d = env
        .ok(
            Some(env.admin),
            "{ user(username: \"alice\") { accessTokens { totalCount } } }",
            json!({}),
        )
        .await;
    assert_eq!(d["user"]["accessTokens"]["totalCount"], 1);

    // Only the owner (or a site admin) may delete it.
    let admin_token = env
        .ok(
            Some(env.admin),
            create,
            json!({ "user": admin_id, "scopes": ["user:all"], "note": "a" }),
        )
        .await["createAccessToken"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    env.err(
        Some(env.alice),
        delete,
        json!({ "byID": admin_token }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        delete,
        json!({ "byID": alice_id }),
        "INVALID_INPUT",
    )
    .await;
    let d = env
        .ok(Some(env.alice), delete, json!({ "byID": token_id }))
        .await;
    assert_eq!(d["deleteAccessToken"]["alwaysNil"], Value::Null);
    let d = env.ok(Some(env.alice), list, json!({})).await;
    assert_eq!(d["currentUser"]["accessTokens"]["totalCount"], 0);
    assert!(
        env.auth
            .authenticate_access_token(&format!("token {secret}"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn set_user_is_site_admin_guards_last_admin() {
    let Some(env) = setup().await else { return };
    let m = "mutation SetUserIsSiteAdmin($userID: ID!, $siteAdmin: Boolean!) {
        setUserIsSiteAdmin(userID: $userID, siteAdmin: $siteAdmin) { alwaysNil } }";
    let admin_id = marshal_id("User", env.admin.get());
    let alice_id = marshal_id("User", env.alice.get());

    env.err(
        Some(env.alice),
        m,
        json!({ "userID": alice_id, "siteAdmin": true }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.admin),
        m,
        json!({ "userID": admin_id, "siteAdmin": false }),
        "FORBIDDEN",
    )
    .await;
    assert!(env.actor(env.admin).await.site_admin);

    env.ok(
        Some(env.admin),
        m,
        json!({ "userID": alice_id, "siteAdmin": true }),
    )
    .await;
    // The cached principal was invalidated: alice is admin right away.
    assert!(env.actor(env.alice).await.site_admin);
    env.ok(Some(env.alice), SITE_CONFIG, json!({})).await;

    // With two admins, one may step down; then alice is the last one.
    env.ok(
        Some(env.admin),
        m,
        json!({ "userID": admin_id, "siteAdmin": false }),
    )
    .await;
    assert!(!env.actor(env.admin).await.site_admin);
    env.err(Some(env.admin), SITE_CONFIG, json!({}), "FORBIDDEN")
        .await;
    env.err(
        Some(env.alice),
        m,
        json!({ "userID": alice_id, "siteAdmin": false }),
        "FORBIDDEN",
    )
    .await;
    let roles: Vec<String> = env
        .db
        .rbac()
        .roles_for_user(env.alice)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert!(roles.iter().any(|r| r == "SITE_ADMINISTRATOR"), "{roles:?}");
}

#[tokio::test]
async fn orgs_and_nodes() {
    let Some(env) = setup().await else { return };
    let create = "mutation($name: String!, $displayName: String) {
        createOrganization(name: $name, displayName: $displayName) { id name displayName } }";
    let d = env
        .ok(
            Some(env.alice),
            create,
            json!({ "name": "acme", "displayName": "Acme" }),
        )
        .await;
    let org_id = d["createOrganization"]["id"].as_str().unwrap().to_owned();
    assert_eq!(d["createOrganization"]["displayName"], "Acme");
    env.err(
        Some(env.alice),
        create,
        json!({ "name": "acme" }),
        "ALREADY_EXISTS",
    )
    .await;
    env.err(None, create, json!({ "name": "other" }), "UNAUTHORIZED")
        .await;

    let list = "{ organizations { nodes { id name displayName } totalCount } }";
    env.err(Some(env.alice), list, json!({}), "FORBIDDEN").await;
    let d = env.ok(Some(env.admin), list, json!({})).await;
    assert_eq!(d["organizations"]["totalCount"], 1);
    assert_eq!(d["organizations"]["nodes"][0]["name"], "acme");

    let d = env
        .ok(
            Some(env.alice),
            "{ currentUser { organizations { nodes { name } } } }",
            json!({}),
        )
        .await;
    assert_eq!(
        d["currentUser"]["organizations"]["nodes"][0]["name"],
        "acme"
    );

    let node = "query($id: ID!) { node(id: $id) { __typename id ... on User { username } ... on Org { name } } }";
    let d = env.ok(Some(env.alice), node, json!({ "id": org_id })).await;
    assert_eq!(d["node"]["__typename"], "Org");
    assert_eq!(d["node"]["name"], "acme");
    let d = env
        .ok(
            Some(env.alice),
            node,
            json!({ "id": marshal_id("User", env.admin.get()) }),
        )
        .await;
    assert_eq!(d["node"]["__typename"], "User");
    assert_eq!(d["node"]["username"], "admin");
    let d = env
        .ok(Some(env.alice), node, json!({ "id": "garbage" }))
        .await;
    assert_eq!(d["node"], Value::Null);
    env.err(None, node, json!({ "id": org_id }), "UNAUTHORIZED")
        .await;
}

#[tokio::test]
async fn depth_limit_and_introspection() {
    let Some(env) = setup().await else { return };
    let mut q = String::from("username");
    for _ in 0..8 {
        q = format!("accessTokens {{ nodes {{ subject {{ {q} }} }} }}");
    }
    let resp = env
        .run(
            Some(env.admin),
            &format!("{{ currentUser {{ {q} }} }}"),
            json!({}),
        )
        .await;
    assert!(!resp.errors.is_empty());
    assert!(
        resp.errors[0]
            .message
            .to_lowercase()
            .contains("nested too deep"),
        "{:?}",
        resp.errors
    );

    let d = env
        .ok(None, "{ __schema { queryType { name } } }", json!({}))
        .await;
    assert_eq!(d["__schema"]["queryType"]["name"], "QueryRoot");
    let sdl = sourcegull::features::graphql_sdl();
    for needle in [
        "type EmptyResponse",
        "alwaysNil: String",
        "interface Node",
        "updateSiteConfiguration(lastID: Int!, input: String!): Boolean!",
    ] {
        assert!(sdl.contains(needle), "{needle} missing from SDL");
    }
}

#[tokio::test]
async fn http_endpoint_post_and_get() {
    let Some(env) = setup().await else { return };
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle());
    let state = AppState::new(
        env.db.clone(),
        env.config.clone(),
        metrics,
        env.auth.clone(),
    );
    let router = sourcegull_http_api::finish(
        sourcegull_http_api::public_routes().merge(sourcegull::features::graphql_routes(&state)),
        state,
    );
    let (_, secret) = env
        .auth
        .create_access_token(
            &env.actor(env.admin).await,
            env.admin,
            &["user:all".into()],
            "t",
            None,
        )
        .await
        .unwrap();
    let auth = format!("token {secret}");

    let call = |req: http::Request<Body>| {
        let router = router.clone();
        async move {
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };

    let body = json!({ "query": "query CurrentUser { currentUser { username } }", "variables": {}, "operationName": "CurrentUser" });
    let (status, v) = call(
        http::Request::post("/.api/graphql?CurrentUser")
            .header("authorization", &auth)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["data"]["currentUser"]["username"], "admin");

    let (status, v) = call(
        http::Request::get("/.api/graphql?query=%7BcurrentUser%7Busername%7D%7D")
            .header("authorization", &auth)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["currentUser"]["username"], "admin");

    // GET never runs mutations.
    let (status, _) = call(
        http::Request::get(
            "/.api/graphql?query=mutation%7BdeleteAccessToken(byID:%22x%22)%7BalwaysNil%7D%7D",
        )
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    let (status, v) = call(
        http::Request::post("/.api/graphql")
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(v["errors"][0]["message"].is_string());
}

const CONTEXT_FIELDS: &str = "id name spec description public autoDefined query updatedAt
    namespace { __typename id } viewerCanManage viewerHasAsDefault viewerHasStarred
    repositories { repository { name } revisions }";

#[tokio::test]
async fn search_contexts_crud_access_stars_and_default() {
    let Some(env) = setup().await else { return };
    let bob = env
        .auth
        .sign_up("bob", "bob@example.com", PW)
        .await
        .expect("bob")
        .id;
    let org = env.db.orgs().create("acme", None).await.unwrap();
    env.db
        .orgs()
        .add_member(org.id, bob, sourcegull_types::OrgRole::Member)
        .await
        .unwrap();
    let repo: i32 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ('gitlab.com/acme/api', '1', 'gitlab', 'https://gitlab.com/') RETURNING id",
    )
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    let repo_id = marshal_id("Repository", i64::from(repo));
    let alice_ns = marshal_id("User", env.alice.get());
    let org_ns = marshal_id("Org", org.id.get());
    let create = format!(
        "mutation($input: SearchContextInput!, $repos: [SearchContextRepositoryRevisionsInput!]!) {{
            createSearchContext(searchContext: $input, repositories: $repos) {{ {CONTEXT_FIELDS} }} }}"
    );
    let input = |name: &str, ns: Option<&str>, public: bool, query: &str| json!({ "name": name, "description": "d", "public": public, "namespace": ns, "query": query });

    // Alice: a private listed context of her own.
    let d = env
        .ok(
            Some(env.alice),
            &create,
            json!({ "input": input("core", Some(&alice_ns), false, ""),
                    "repos": [{ "repositoryID": repo_id, "revisions": ["main", "HEAD"] }] }),
        )
        .await;
    let core = &d["createSearchContext"];
    assert_eq!(core["spec"], "@alice/core");
    assert_eq!(core["namespace"]["__typename"], "User");
    assert_eq!(core["viewerCanManage"], true);
    assert_eq!(
        core["repositories"],
        json!([{ "repository": { "name": "gitlab.com/acme/api" }, "revisions": ["HEAD", "main"] }])
    );
    let core_id = core["id"].as_str().unwrap().to_owned();

    // Writes: not for others' namespaces, public needs a site admin unless
    // it is one's own, instance-level needs a site admin.
    env.err(
        Some(bob),
        &create,
        json!({ "input": input("x", Some(&alice_ns), false, ""), "repos": [] }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        &create,
        json!({ "input": input("x", Some(&org_ns), false, ""), "repos": [] }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        &create,
        json!({ "input": input("x", None, true, ""), "repos": [] }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        None,
        &create,
        json!({ "input": input("x", None, true, ""), "repos": [] }),
        "UNAUTHORIZED",
    )
    .await;
    // Validation.
    for bad in [
        input("has space", Some(&alice_ns), false, ""),
        input(&"x".repeat(33), Some(&alice_ns), false, ""),
        // Patterns and non-repository fields are not allowed in a context query.
        input("q", Some(&alice_ns), false, "repo:a foo"),
        input("q", Some(&alice_ns), false, "type:diff"),
    ] {
        env.err(
            Some(env.alice),
            &create,
            json!({ "input": bad, "repos": [] }),
            "VALIDATION",
        )
        .await;
    }
    env.err(
        Some(env.admin),
        &create,
        json!({ "input": input("global", None, true, ""), "repos": [] }),
        "VALIDATION",
    )
    .await;
    env.err(
        Some(env.alice),
        &create,
        json!({ "input": input("core", Some(&alice_ns), false, ""), "repos": [] }),
        "ALREADY_EXISTS",
    )
    .await;

    // Bob: an org context defined by a query; an admin: a public instance context.
    let d = env
        .ok(Some(bob), &create, json!({ "input": input("web", Some(&org_ns), false, "repo:web fork:yes"), "repos": [] }))
        .await;
    assert_eq!(d["createSearchContext"]["spec"], "@acme/web");
    assert_eq!(d["createSearchContext"]["query"], "repo:web fork:yes");
    let web_id = d["createSearchContext"]["id"].as_str().unwrap().to_owned();
    env.ok(
        Some(env.admin),
        &create,
        json!({ "input": input("all-api", None, true, "repo:api"), "repos": [] }),
    )
    .await;

    // Reads: global first, then what the viewer may see.
    let list = "query($q: String, $ns: [ID]) { searchContexts(query: $q, namespaces: $ns) {
        nodes { spec viewerHasAsDefault viewerHasStarred } totalCount pageInfo { hasNextPage } } }";
    let specs = |d: &Value| {
        d["searchContexts"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["spec"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let d = env.ok(Some(env.alice), list, json!({})).await;
    assert_eq!(specs(&d), ["global", "all-api", "@alice/core"]);
    assert_eq!(d["searchContexts"]["totalCount"], 3);
    let d = env.ok(Some(bob), list, json!({})).await;
    assert_eq!(specs(&d), ["global", "all-api", "@acme/web"]);
    let d = env.ok(Some(env.admin), list, json!({})).await;
    assert_eq!(specs(&d), ["global", "all-api"]);
    let d = env.ok(Some(env.alice), list, json!({ "q": "core" })).await;
    assert_eq!(specs(&d), ["@alice/core"]);
    let d = env.ok(Some(bob), list, json!({ "ns": [org_ns] })).await;
    assert_eq!(specs(&d), ["@acme/web"]);
    let d = env.ok(Some(bob), list, json!({ "ns": [null] })).await;
    assert_eq!(specs(&d), ["global", "all-api"]);
    // Paging through global and stored contexts.
    let paged = "query($after: String) { searchContexts(first: 1, after: $after) {
        nodes { spec } pageInfo { hasNextPage endCursor } } }";
    let mut after = Value::Null;
    let mut seen = Vec::new();
    loop {
        let d = env
            .ok(Some(env.alice), paged, json!({ "after": after }))
            .await;
        seen.extend(specs(&d));
        let info = &d["searchContexts"]["pageInfo"];
        if info["hasNextPage"] != true {
            break;
        }
        after = info["endCursor"].clone();
    }
    assert_eq!(seen, ["global", "all-api", "@alice/core"]);

    let by_spec = format!(
        "query($spec: String!) {{ searchContextBySpec(spec: $spec) {{ {CONTEXT_FIELDS} }}
        isSearchContextAvailable(spec: $spec) }}"
    );
    let d = env
        .ok(Some(bob), &by_spec, json!({ "spec": "@alice/core" }))
        .await;
    assert_eq!(d["searchContextBySpec"], Value::Null);
    assert_eq!(d["isSearchContextAvailable"], false);
    let d = env
        .ok(Some(env.alice), &by_spec, json!({ "spec": "@Alice/core" }))
        .await;
    assert_eq!(d["searchContextBySpec"]["id"], core_id.as_str());
    let d = env
        .ok(Some(env.alice), &by_spec, json!({ "spec": "global" }))
        .await;
    assert_eq!(d["searchContextBySpec"]["autoDefined"], true);
    assert_eq!(d["searchContextBySpec"]["viewerHasAsDefault"], true);
    // node(id:) honours the same rule.
    let node = "query($id: ID!) { node(id: $id) { ... on SearchContext { spec } } }";
    let d = env.ok(Some(bob), node, json!({ "id": core_id })).await;
    assert_eq!(d["node"], Value::Null);
    let d = env
        .ok(Some(env.alice), node, json!({ "id": core_id }))
        .await;
    assert_eq!(d["node"]["spec"], "@alice/core");

    // Stars and default, for oneself only, on readable contexts.
    let alice_id = marshal_id("User", env.alice.get());
    let star = "mutation($c: ID!, $u: ID!) { createSearchContextStar(searchContextID: $c, userID: $u) { alwaysNil } }";
    let set_default = "mutation($c: ID!, $u: ID!) { setDefaultSearchContext(searchContextID: $c, userID: $u) { alwaysNil } }";
    env.err(
        Some(bob),
        star,
        json!({ "c": core_id, "u": alice_id }),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        star,
        json!({ "c": web_id, "u": alice_id }),
        "NOT_FOUND",
    )
    .await;
    env.ok(
        Some(env.alice),
        star,
        json!({ "c": core_id, "u": alice_id }),
    )
    .await;
    env.ok(
        Some(env.alice),
        set_default,
        json!({ "c": core_id, "u": alice_id }),
    )
    .await;
    let d = env.ok(Some(env.alice), list, json!({})).await;
    assert_eq!(specs(&d), ["global", "@alice/core", "all-api"]);
    assert_eq!(d["searchContexts"]["nodes"][0]["viewerHasAsDefault"], false);
    assert_eq!(d["searchContexts"]["nodes"][1]["viewerHasAsDefault"], true);
    assert_eq!(d["searchContexts"]["nodes"][1]["viewerHasStarred"], true);
    let d = env
        .ok(
            Some(env.alice),
            "{ defaultSearchContext { spec } }",
            json!({}),
        )
        .await;
    assert_eq!(d["defaultSearchContext"]["spec"], "@alice/core");
    let global_id = marshal_id("SearchContext", 0);
    env.ok(
        Some(env.alice),
        set_default,
        json!({ "c": global_id, "u": alice_id }),
    )
    .await;
    let d = env
        .ok(
            Some(env.alice),
            "{ defaultSearchContext { spec } }",
            json!({}),
        )
        .await;
    assert_eq!(d["defaultSearchContext"]["spec"], "global");
    env.ok(
        Some(env.alice),
        "mutation($c: ID!, $u: ID!) { deleteSearchContextStar(searchContextID: $c, userID: $u) { alwaysNil } }",
        json!({ "c": core_id, "u": alice_id }),
    )
    .await;

    // Update and delete.
    let update = "mutation($id: ID!, $input: SearchContextEditInput!, $repos: [SearchContextRepositoryRevisionsInput!]!) {
        updateSearchContext(id: $id, searchContext: $input, repositories: $repos) { spec public query repositories { revisions } } }";
    let edit = json!({ "name": "core2", "description": "", "public": true, "query": "" });
    env.err(
        Some(bob),
        update,
        json!({ "id": core_id, "input": edit, "repos": [] }),
        "NOT_FOUND",
    )
    .await;
    let d = env
        .ok(
            Some(env.alice),
            update,
            json!({ "id": core_id, "input": edit,
            "repos": [{ "repositoryID": repo_id, "revisions": [] }] }),
        )
        .await;
    assert_eq!(d["updateSearchContext"]["spec"], "@alice/core2");
    assert_eq!(
        d["updateSearchContext"]["repositories"][0]["revisions"],
        json!(["HEAD"])
    );
    // Now public: bob can read it, not change it.
    env.err(
        Some(bob),
        update,
        json!({ "id": core_id, "input": edit, "repos": [] }),
        "FORBIDDEN",
    )
    .await;
    let delete = "mutation($id: ID!) { deleteSearchContext(id: $id) { alwaysNil } }";
    env.err(Some(bob), delete, json!({ "id": core_id }), "FORBIDDEN")
        .await;
    // Site admins manage public contexts.
    env.ok(Some(env.admin), delete, json!({ "id": core_id }))
        .await;
    env.err(
        Some(env.alice),
        delete,
        json!({ "id": core_id }),
        "NOT_FOUND",
    )
    .await;
}

#[tokio::test]
async fn saved_searches_crud_and_access() {
    let Some(env) = setup().await else { return };
    let bob = env
        .auth
        .sign_up("bob", "bob@example.com", PW)
        .await
        .expect("bob")
        .id;
    let org = env.db.orgs().create("acme", None).await.unwrap();
    env.db
        .orgs()
        .add_member(org.id, bob, sourcegull_types::OrgRole::Member)
        .await
        .unwrap();
    let alice_ns = marshal_id("User", env.alice.get());
    let bob_ns = marshal_id("User", bob.get());
    let org_ns = marshal_id("Org", org.id.get());
    let fields =
        "id description query draft visibility owner { __typename id } createdBy { username }
        updatedBy { username } url viewerCanAdminister";
    let create = format!(
        "mutation($input: SavedSearchInput!) {{ createSavedSearch(input: $input) {{ {fields} }} }}"
    );
    let input = |owner: &str, description: &str, query: &str, visibility: &str| {
        json!({ "input": { "owner": owner, "description": description, "query": query,
                           "draft": false, "visibility": visibility } })
    };

    let d = env
        .ok(
            Some(env.alice),
            &create,
            input(&alice_ns, "TODOs", "TODO patterntype:keyword", "SECRET"),
        )
        .await;
    let todos = &d["createSavedSearch"];
    assert_eq!(todos["owner"]["__typename"], "User");
    assert_eq!(todos["createdBy"]["username"], "alice");
    assert_eq!(todos["viewerCanAdminister"], true);
    assert!(
        todos["url"]
            .as_str()
            .unwrap()
            .starts_with("/saved-searches/")
    );
    let todos_id = todos["id"].as_str().unwrap().to_owned();
    let d = env
        .ok(
            Some(bob),
            &create,
            input(&org_ns, "Panics", "panic( patterntype:literal", "SECRET"),
        )
        .await;
    let panics_id = d["createSavedSearch"]["id"].as_str().unwrap().to_owned();

    env.err(
        Some(env.alice),
        &create,
        input(&org_ns, "x", "x patterntype:keyword", "SECRET"),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(bob),
        &create,
        input(&alice_ns, "x", "x patterntype:keyword", "SECRET"),
        "FORBIDDEN",
    )
    .await;
    env.err(
        Some(env.alice),
        &create,
        input(&alice_ns, "x", "no pattern type", "SECRET"),
        "VALIDATION",
    )
    .await;
    env.err(
        Some(env.alice),
        &create,
        input(&alice_ns, "", "x patterntype:keyword", "SECRET"),
        "VALIDATION",
    )
    .await;
    // Only site admins publish.
    env.err(
        Some(env.alice),
        &create,
        input(&alice_ns, "x", "x patterntype:keyword", "PUBLIC"),
        "FORBIDDEN",
    )
    .await;
    let d = env
        .ok(
            Some(env.admin),
            &create,
            input(&bob_ns, "Unsafe", "unsafe patterntype:keyword", "PUBLIC"),
        )
        .await;
    assert_eq!(d["createSavedSearch"]["visibility"], "PUBLIC");

    let list = "query($owner: ID, $aff: Boolean, $q: String) {
        savedSearches(owner: $owner, viewerIsAffiliated: $aff, query: $q, orderBy: SAVED_SEARCH_DESCRIPTION) {
        nodes { description } totalCount } }";
    let names = |d: &Value| {
        d["savedSearches"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["description"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let d = env.ok(Some(env.alice), list, json!({ "aff": true })).await;
    assert_eq!(names(&d), ["TODOs", "Unsafe"]);
    let d = env.ok(Some(bob), list, json!({ "aff": true })).await;
    assert_eq!(names(&d), ["Panics", "Unsafe"]);
    let d = env.ok(Some(bob), list, json!({ "owner": org_ns })).await;
    assert_eq!(names(&d), ["Panics"]);
    let d = env
        .ok(Some(env.alice), list, json!({ "aff": true, "q": "todo" }))
        .await;
    assert_eq!(names(&d), ["TODOs"]);
    env.err(
        Some(env.alice),
        list,
        json!({ "owner": org_ns }),
        "FORBIDDEN",
    )
    .await;
    env.err(Some(env.alice), list, json!({}), "FORBIDDEN").await;
    let d = env.ok(Some(env.admin), list, json!({})).await;
    assert_eq!(d["savedSearches"]["totalCount"], 3);

    // Paging both ways (`ConnectionPageInfo`).
    let paged = "query($first: Int, $last: Int, $after: String, $before: String) {
        savedSearches(first: $first, last: $last, after: $after, before: $before,
            orderBy: SAVED_SEARCH_DESCRIPTION) {
        nodes { description } totalCount
        pageInfo { startCursor endCursor hasNextPage hasPreviousPage } } }";
    let d = env.ok(Some(env.admin), paged, json!({ "last": 2 })).await;
    assert_eq!(names(&d), ["TODOs", "Unsafe"]);
    let page = &d["savedSearches"]["pageInfo"];
    assert_eq!(page["hasPreviousPage"], true);
    assert_eq!(page["hasNextPage"], false);
    let d = env
        .ok(
            Some(env.admin),
            paged,
            json!({ "last": 2, "before": page["startCursor"] }),
        )
        .await;
    assert_eq!(names(&d), ["Panics"]);
    assert_eq!(d["savedSearches"]["pageInfo"]["hasPreviousPage"], false);
    let d = env.ok(Some(env.admin), paged, json!({ "first": 1 })).await;
    assert_eq!(names(&d), ["Panics"]);
    let d = env
        .ok(
            Some(env.admin),
            paged,
            json!({ "first": 5, "after": d["savedSearches"]["pageInfo"]["endCursor"] }),
        )
        .await;
    assert_eq!(names(&d), ["TODOs", "Unsafe"]);
    env.err(
        Some(env.admin),
        paged,
        json!({ "first": 1, "last": 1 }),
        "INVALID_INPUT",
    )
    .await;

    let node = "query($id: ID!) { node(id: $id) { ... on SavedSearch { description } } }";
    let d = env.ok(Some(bob), node, json!({ "id": todos_id })).await;
    assert_eq!(d["node"], Value::Null);
    let d = env
        .ok(Some(env.admin), node, json!({ "id": todos_id }))
        .await;
    assert_eq!(d["node"]["description"], "TODOs");

    // Update, visibility, transfer, delete.
    let update = "mutation($id: ID!, $input: SavedSearchUpdateInput!) {
        updateSavedSearch(id: $id, input: $input) { description draft updatedBy { username } } }";
    let edit =
        json!({ "description": "Panics!", "query": "panic patterntype:keyword", "draft": true });
    env.err(
        Some(env.alice),
        update,
        json!({ "id": panics_id, "input": edit }),
        "NOT_FOUND",
    )
    .await;
    let d = env
        .ok(Some(bob), update, json!({ "id": panics_id, "input": edit }))
        .await;
    assert_eq!(d["updateSavedSearch"]["description"], "Panics!");
    assert_eq!(d["updateSavedSearch"]["updatedBy"]["username"], "bob");
    let visibility = "mutation($id: ID!, $v: SavedSearchVisibility!) {
        changeSavedSearchVisibility(id: $id, newVisibility: $v) { visibility } }";
    env.err(
        Some(bob),
        visibility,
        json!({ "id": panics_id, "v": "PUBLIC" }),
        "FORBIDDEN",
    )
    .await;
    let d = env
        .ok(
            Some(env.admin),
            visibility,
            json!({ "id": panics_id, "v": "PUBLIC" }),
        )
        .await;
    assert_eq!(d["changeSavedSearchVisibility"]["visibility"], "PUBLIC");
    // Public now: alice sees it but cannot change it.
    env.err(
        Some(env.alice),
        update,
        json!({ "id": panics_id, "input": edit }),
        "FORBIDDEN",
    )
    .await;
    let transfer = "mutation($id: ID!, $to: ID!) {
        transferSavedSearchOwnership(id: $id, newOwner: $to) { owner { id } } }";
    env.err(
        Some(bob),
        transfer,
        json!({ "id": panics_id, "to": alice_ns }),
        "FORBIDDEN",
    )
    .await;
    let d = env
        .ok(
            Some(bob),
            transfer,
            json!({ "id": panics_id, "to": bob_ns }),
        )
        .await;
    assert_eq!(
        d["transferSavedSearchOwnership"]["owner"]["id"],
        bob_ns.as_str()
    );
    let delete = "mutation($id: ID!) { deleteSavedSearch(id: $id) { alwaysNil } }";
    env.err(
        Some(env.alice),
        delete,
        json!({ "id": panics_id }),
        "FORBIDDEN",
    )
    .await;
    env.ok(Some(bob), delete, json!({ "id": panics_id })).await;
    env.err(Some(bob), delete, json!({ "id": panics_id }), "NOT_FOUND")
        .await;
}

#[tokio::test]
async fn access_requests_are_admin_only_and_approval_creates_invite() {
    let Some(env) = setup().await else { return };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO access_requests (name, email, additional_info) VALUES ('Bob', 'bob@example.com', 'team') RETURNING id",
    ).fetch_one(env.db.pool()).await.unwrap();
    let list = "{ accessRequests(status: PENDING) { totalCount nodes { id email } } accessRequestsCount(status: PENDING) }";
    env.err(None, list, json!({}), "UNAUTHORIZED").await;
    env.err(Some(env.alice), list, json!({}), "FORBIDDEN").await;
    let data = env.ok(Some(env.admin), list, json!({})).await;
    assert_eq!(data["accessRequestsCount"], 1);
    assert_eq!(
        data["accessRequests"]["nodes"][0]["email"],
        "bob@example.com"
    );
    let mutation = "mutation($id: ID!) { setAccessRequestStatus(id: $id, status: APPROVED) { resetUrl request { status } } }";
    let vars = json!({"id": marshal_id("AccessRequest", id)});
    env.err(Some(env.alice), mutation, vars.clone(), "FORBIDDEN")
        .await;
    let data = env.ok(Some(env.admin), mutation, vars.clone()).await;
    assert_eq!(
        data["setAccessRequestStatus"]["request"]["status"],
        "APPROVED"
    );
    assert!(
        data["setAccessRequestStatus"]["resetUrl"]
            .as_str()
            .unwrap()
            .contains("/set-password?token=")
    );
    assert!(
        env.db
            .users()
            .get_by_username("bob")
            .await
            .unwrap()
            .is_some()
    );
    env.err(Some(env.admin), mutation, vars, "CONFLICT").await;
    let rejected_id: i64 = sqlx::query_scalar(
        "INSERT INTO access_requests (name, email) VALUES ('Carol', 'carol@example.com') RETURNING id",
    ).fetch_one(env.db.pool()).await.unwrap();
    let rejected = env.ok(Some(env.admin),
        "mutation($id: ID!) { setAccessRequestStatus(id: $id, status: REJECTED) { resetUrl request { status } } }",
        json!({"id": marshal_id("AccessRequest", rejected_id)})).await;
    assert_eq!(
        rejected["setAccessRequestStatus"]["request"]["status"],
        "REJECTED"
    );
    assert!(rejected["setAccessRequestStatus"]["resetUrl"].is_null());
    assert!(
        env.db
            .users()
            .get_by_username("carol")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn approved_access_request_emails_a_working_setup_link() {
    let Some(mut env) = setup().await else { return };
    let mut config = env.config.load().value.clone();
    config.external_url = Some("https://sourcegull.example".into());
    config.email_address = Some("sourcegull@example.com".into());
    config.email_smtp = Some(SmtpConfig {
        host: "localhost".into(),
        port: 25,
        username: None,
        password: None,
        authentication: SmtpAuthentication::None,
        no_verify_tls: false,
        encryption: SmtpEncryption::None,
    });
    env.config.store(2, config);
    let sender = MemoryEmailSender::default();
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let state = AppState::new(
        env.db.clone(),
        env.config.clone(),
        sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle()),
        env.auth.clone(),
    )
    .with_email_sender(Arc::new(sender.clone()));
    env.schema = build_schema(Services::from_state(&state));
    let requests = env.db.access_requests();
    requests.create("Bob", "bob@example.com", "").await.unwrap();
    let id = requests
        .list(Some(sourcegull_db::AccessRequestStatus::Pending), 0, 10)
        .await
        .unwrap()[0]
        .id;
    let data = env
        .ok(
            Some(env.admin),
            "mutation($id: ID!) { setAccessRequestStatus(id: $id, status: APPROVED) { resetUrl } }",
            json!({"id": marshal_id("AccessRequest", id)}),
        )
        .await;
    let link = data["setAccessRequestStatus"]["resetUrl"].as_str().unwrap();
    assert!(sender.messages()[0].text.contains(link));
    let token = link.split("token=").nth(1).unwrap();
    let app = sourcegull_http_api::finish(sourcegull_http_api::auth_routes(), state);
    let request = http::Request::post("/.api/set-invite-password")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"token": token, "password": PW}).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn admins_can_issue_reset_links_and_manage_email_verification() {
    let Some(env) = setup().await else { return };
    let user = marshal_id("User", env.alice.get());
    env.err(
        Some(env.alice),
        "mutation($u: ID!) { randomizeUserPassword(user: $u) { resetPasswordURL } }",
        json!({"u": user.clone()}),
        "FORBIDDEN",
    )
    .await;
    let data = env
        .ok(
            Some(env.admin),
            "mutation($u: ID!) { randomizeUserPassword(user: $u) { resetPasswordURL } }",
            json!({"u": user.clone()}),
        )
        .await;
    let link = data["randomizeUserPassword"]["resetPasswordURL"]
        .as_str()
        .unwrap();
    assert!(link.contains("/reset-password?userID="));
    let code = link.split("code=").nth(1).unwrap();
    assert!(
        env.db
            .password_resets()
            .redeem(
                env.alice,
                &sourcegull_crypto::session::hash_session_id(code)
            )
            .await
            .unwrap()
    );
    assert!(
        !env.db
            .password_resets()
            .redeem(
                env.alice,
                &sourcegull_crypto::session::hash_session_id(code)
            )
            .await
            .unwrap()
    );
    env.ok(Some(env.admin),
        "mutation($u: ID!) { setUserEmailVerified(user: $u, email: \"alice@example.com\", verified: true) { alwaysNil } }",
        json!({"u": user.clone()})).await;
    assert!(env.db.user_emails().list(env.alice).await.unwrap()[0].is_verified());
    env.ok(Some(env.admin),
        "mutation($u: ID!) { setUserEmailVerified(user: $u, email: \"alice@example.com\", verified: false) { alwaysNil } }",
        json!({"u": user})).await;
    assert!(!env.db.user_emails().list(env.alice).await.unwrap()[0].is_verified());
}
