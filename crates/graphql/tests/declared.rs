//! Objects owned by the configuration-as-code files over GraphQL against the test Postgres
//! (`SOURCEGULL_TEST_DATABASE_URL`): writes to declared objects, creation in exclusive kinds,
//! global settings from a file and declared site admins are refused with `READ_ONLY`;
//! `managedBy`, `readOnly`, `declaredConfigFiles` and the status message show it.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{
    Db, FeatureFlagValue, GLOBAL_SETTINGS_SOURCE, NewAccount, NewUser, RESOURCES_SOURCE, migrate,
};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_outbound_webhooks::store::{NewOutboundWebhook, OutboundWebhookStore};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

struct Env {
    _t: TestDb,
    db: Db,
    auth: Authenticator,
    schema: SourcegullSchema,
    admin: UserId,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
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
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: Some(keyring),
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        auth,
        schema,
        admin,
    })
}

impl Env {
    async fn run(&self, user: UserId, query: &str, vars: Value) -> Response {
        let actor = self
            .auth
            .actor_for(user, AuthVia::Session)
            .await
            .unwrap()
            .unwrap();
        let req = Request::new(query).variables(Variables::from_json(vars));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn ok(&self, query: &str, vars: Value) -> Value {
        let resp = self.run(self.admin, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn read_only(&self, query: &str, vars: Value) -> String {
        self.read_only_as(self.admin, query, vars).await
    }

    async fn read_only_as(&self, user: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(user, query, vars).await;
        let err = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        let code = err
            .extensions
            .as_ref()
            .and_then(|e| e.get("code"))
            .and_then(|c| c.clone().into_json().ok())
            .and_then(|c| c.as_str().map(str::to_owned))
            .unwrap_or_default();
        assert_eq!(code, "READ_ONLY", "{query}: {}", err.message);
        err.message.clone()
    }

    async fn declare(&self, kinds: &[&str], exclusive: bool) {
        let kinds: Vec<String> = kinds.iter().map(|k| (*k).to_owned()).collect();
        self.db
            .declared()
            .record_applied(
                RESOURCES_SOURCE,
                "/etc/sourcegull/resources.jsonc",
                &kinds,
                exclusive,
                b"h",
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn declared_objects_are_read_only() {
    let Some(env) = setup().await else { return };
    let flags = env.db.feature_flags();
    flags
        .create("declared", FeatureFlagValue::Bool(true))
        .await
        .unwrap();
    flags
        .create("ui", FeatureFlagValue::Bool(true))
        .await
        .unwrap();
    env.declare(&["featureFlags", "outboundWebhooks"], false)
        .await;
    env.db
        .declared()
        .set_object("featureFlags", "declared", "declared", b"h")
        .await
        .unwrap();

    let msg = env
        .read_only(
            "mutation { updateFeatureFlag(name: \"declared\", value: false) { __typename } }",
            json!({}),
        )
        .await;
    assert!(msg.contains("featureFlags[declared]"), "{msg}");
    env.read_only(
        "mutation { deleteFeatureFlag(name: \"declared\") { alwaysNil } }",
        json!({}),
    )
    .await;
    // Objects the file does not list stay editable, and new ones can be created.
    env.ok(
        "mutation { updateFeatureFlag(name: \"ui\", value: false) { __typename } }",
        json!({}),
    )
    .await;
    env.ok(
        "mutation { createFeatureFlag(name: \"new\", value: true) { __typename } }",
        json!({}),
    )
    .await;

    let data = env
        .ok(
            "{ a: featureFlag(name: \"declared\") { ... on FeatureFlagBoolean { managedBy } }
               b: featureFlag(name: \"ui\") { ... on FeatureFlagBoolean { managedBy } } }",
            json!({}),
        )
        .await;
    assert_eq!(data["a"]["managedBy"], "featureFlags[declared]");
    assert_eq!(data["b"]["managedBy"], Value::Null);

    let hook = OutboundWebhookStore::new(&env.db, None)
        .list(&Default::default())
        .await
        .unwrap();
    assert!(hook.is_empty());
    let keyring = Arc::new(
        env.db
            .encryption_keys()
            .load_keyring(Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap()))
            .await
            .unwrap(),
    );
    let hook = OutboundWebhookStore::new(&env.db, Some(keyring))
        .create(&NewOutboundWebhook {
            url: "https://hooks.example.com/a".into(),
            secret: "s".into(),
            event_types: vec![sourcegull_outbound_webhooks::store::ScopedEventType {
                event_type: "batch_change:apply".into(),
                scope: None,
            }],
            created_by: None,
        })
        .await
        .unwrap();
    env.db
        .declared()
        .set_object("outboundWebhooks", "audit", &hook.id.to_string(), b"h")
        .await
        .unwrap();
    let id = marshal_id("OutboundWebhook", hook.id);
    env.read_only(
        "mutation($id: ID!) { deleteOutboundWebhook(id: $id) { alwaysNil } }",
        json!({ "id": id }),
    )
    .await;

    let files = env
        .ok(
            "{ site { declaredConfigFiles { source path kinds exclusive error } } }",
            json!({}),
        )
        .await;
    assert_eq!(
        files["site"]["declaredConfigFiles"][0]["source"],
        "resources"
    );
    assert_eq!(
        files["site"]["declaredConfigFiles"][0]["kinds"],
        json!(["featureFlags", "outboundWebhooks"])
    );
}

#[tokio::test]
async fn exclusive_kinds_refuse_creation() {
    let Some(env) = setup().await else { return };
    env.declare(&["featureFlags"], true).await;
    let msg = env
        .read_only(
            "mutation { createFeatureFlag(name: \"x\", value: true) { __typename } }",
            json!({}),
        )
        .await;
    assert!(msg.contains("exclusively"), "{msg}");
    // Other kinds are not affected.
    env.ok(
        "mutation { createRole(name: \"r\", permissions: []) { name } }",
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn global_settings_from_a_file_are_read_only() {
    let Some(env) = setup().await else { return };
    let site = marshal_id_string();
    let query = "mutation($s: ID!) { settingsMutation(input: { subject: $s, lastID: null }) {
        overwriteSettings(contents: \"{}\") { empty { alwaysNil } } } }";
    env.ok(query, json!({ "s": site })).await;
    env.db
        .declared()
        .record_applied(
            GLOBAL_SETTINGS_SOURCE,
            "/etc/sourcegull/settings.jsonc",
            &[],
            false,
            b"h",
        )
        .await
        .unwrap();
    let latest = env
        .ok(
            "{ site { viewerCanAdminister latestSettings { id readOnly } } }",
            json!({}),
        )
        .await;
    assert_eq!(latest["site"]["viewerCanAdminister"], false);
    assert_eq!(latest["site"]["latestSettings"]["readOnly"], true);
    let query =
        "mutation($s: ID!, $last: Int) { settingsMutation(input: { subject: $s, lastID: $last }) {
        overwriteSettings(contents: \"{}\") { empty { alwaysNil } } } }";
    let msg = env
        .read_only(
            query,
            json!({ "s": site, "last": latest["site"]["latestSettings"]["id"] }),
        )
        .await;
    assert!(msg.contains("SOURCEGULL_GLOBAL_SETTINGS_FILE"), "{msg}");
}

fn marshal_id_string() -> String {
    sourcegull_services::relay::marshal_string_id("Site", "site")
}

#[tokio::test]
async fn declared_site_admins_cannot_be_demoted() {
    let Some(env) = setup().await else { return };
    let alice = env
        .db
        .accounts()
        .create(&NewAccount {
            user: NewUser {
                username: "alice".into(),
                ..Default::default()
            },
            email: Some("alice@example.com".into()),
            email_verified: true,
            only_if_first: false,
        })
        .await
        .unwrap()
        .user
        .id;
    env.db.users().set_site_admin(alice, true).await.unwrap();
    env.declare(&["siteAdmins"], false).await;
    env.db
        .declared()
        .set_site_admins(Some(&["alice@example.com".to_owned()]))
        .await
        .unwrap();
    let query = "mutation($u: ID!, $a: Boolean!) { setUserIsSiteAdmin(userID: $u, siteAdmin: $a) { alwaysNil } }";
    let alice_id = marshal_id("User", alice.0);
    let admin_id = marshal_id("User", env.admin.0);
    let msg = env
        .read_only(query, json!({ "u": alice_id, "a": false }))
        .await;
    assert!(msg.contains("siteAdmins"), "{msg}");
    // Verifying or unverifying a declared address is its owner's business.
    let verify = "mutation($u: ID!) { setUserEmailVerified(user: $u, email: \"ALICE@example.com\", verified: false) { alwaysNil } }";
    env.read_only(verify, json!({ "u": alice_id })).await;

    // Admins the file does not list can still be demoted.
    env.ok(query, json!({ "u": admin_id, "a": false })).await;

    // Exclusive: no changes through the API at all.
    env.declare(&["siteAdmins"], true).await;
    env.read_only_as(alice, query, json!({ "u": admin_id, "a": true }))
        .await;
}

#[tokio::test]
async fn errors_show_in_status_messages() {
    let Some(env) = setup().await else { return };
    env.db
        .declared()
        .record_error(
            RESOURCES_SOURCE,
            "/etc/sourcegull/resources.jsonc",
            "connections[a].config.token: references environment variable X, which is not set",
        )
        .await
        .unwrap();
    let data = env
        .ok(
            "{ statusMessages { __typename ... on DeclaredConfigError { message source path } } }",
            json!({}),
        )
        .await;
    let messages = data["statusMessages"].as_array().unwrap();
    let m = messages
        .iter()
        .find(|m| m["__typename"] == "DeclaredConfigError")
        .expect("message");
    assert!(
        m["message"]
            .as_str()
            .unwrap()
            .contains("SOURCEGULL_RESOURCES_FILE")
    );
    assert_eq!(m["source"], "resources");
}
