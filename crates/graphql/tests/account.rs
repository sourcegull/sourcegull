//! Account self-service (`updateUser`, `updatePassword`, the email mutations) and site admin
//! deactivation, against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`).

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams, SessionToken};
use sourcegull_config::{AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig};
use sourcegull_db::{Db, ScimWrite, migrate};
use sourcegull_graphql::account::CurrentSession;
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::marshal_id;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

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
    async fn run_in(
        &self,
        actor: UserId,
        session: Option<SessionToken>,
        query: &str,
        vars: Value,
    ) -> Response {
        let actor = self
            .auth
            .actor_for(actor, AuthVia::Session)
            .await
            .expect("actor")
            .expect("user exists");
        let req = Request::new(query)
            .variables(Variables::from_json(vars))
            .data(CurrentSession(session));
        sourcegull_graphql::execute(&self.schema, req, Some(actor)).await
    }

    async fn ok(&self, actor: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run_in(actor, None, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().expect("json")
    }

    async fn err(&self, actor: UserId, query: &str, vars: Value, code: &str) -> String {
        let resp = self.run_in(actor, None, query, vars).await;
        assert_eq!(
            error_code(&resp).as_deref(),
            Some(code),
            "{query}: {:?}",
            resp.errors
        );
        resp.errors[0].message.clone()
    }

    fn set_username_changes(&self, enabled: bool) {
        let cur = self.config.load();
        let mut value = cur.value.clone();
        value.auth_enable_username_changes = enabled;
        self.config.store(cur.version + 1, value);
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

fn uid(id: UserId) -> String {
    marshal_id("User", id.get())
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
    let admin = auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .expect("admin");
    let alice = auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .expect("alice");
    let services = Services {
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
        authz: sourcegull_authz::Checker::new(db.clone(), config.clone()),
        app: None,
    };
    let schema = build_schema(services);
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

const UPDATE_PASSWORD: &str = "mutation($old: String, $new: String!) {
    updatePassword(oldPassword: $old, newPassword: $new) { alwaysNil } }";

#[tokio::test]
async fn update_password_checks_old_password_and_ends_other_sessions() {
    let Some(env) = setup().await else { return };
    let current = env.auth.start_session(env.alice).await.unwrap();
    let other = env.auth.start_session(env.alice).await.unwrap();
    let admin_session = env.auth.start_session(env.admin).await.unwrap();

    let wrong = env
        .run_in(
            env.alice,
            Some(current.clone()),
            UPDATE_PASSWORD,
            json!({"old": "not my password", "new": "a brand new password"}),
        )
        .await;
    assert_eq!(
        error_code(&wrong).as_deref(),
        Some("INVALID_INPUT"),
        "{:?}",
        wrong.errors
    );
    let missing = env
        .run_in(
            env.alice,
            None,
            UPDATE_PASSWORD,
            json!({"new": "a brand new password"}),
        )
        .await;
    assert_eq!(error_code(&missing).as_deref(), Some("INVALID_INPUT"));
    assert!(env.auth.sign_in("alice", PW, None).await.is_ok());

    let resp = env
        .run_in(
            env.alice,
            Some(current.clone()),
            UPDATE_PASSWORD,
            json!({"old": PW, "new": "a brand new password"}),
        )
        .await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert!(env.auth.sign_in("alice", PW, None).await.is_err());
    assert!(
        env.auth
            .sign_in("alice", "a brand new password", None)
            .await
            .is_ok()
    );
    let sessions = env.auth.sessions();
    assert!(sessions.get(&current).await.unwrap().is_some());
    assert!(sessions.get(&other).await.unwrap().is_none());
    assert!(sessions.get(&admin_session).await.unwrap().is_some());

    // Without a password (e.g. an external sign-in account) no old password is needed.
    env.db
        .users()
        .set_password_hash(env.alice, None)
        .await
        .unwrap();
    env.ok(
        env.alice,
        UPDATE_PASSWORD,
        json!({"new": "set a first password"}),
    )
    .await;
    assert!(
        env.auth
            .sign_in("alice", "set a first password", None)
            .await
            .is_ok()
    );

    // Built-in sign-in disabled.
    let cur = env.config.load();
    let mut value = cur.value.clone();
    value.auth_providers.clear();
    env.config.store(cur.version + 1, value);
    env.err(
        env.alice,
        UPDATE_PASSWORD,
        json!({"old": "set a first password", "new": "another new password"}),
        "FORBIDDEN",
    )
    .await;
}

const ADD: &str =
    "mutation($u: ID!, $e: String!) { addUserEmail(user: $u, email: $e) { alwaysNil } }";
const REMOVE: &str =
    "mutation($u: ID!, $e: String!) { removeUserEmail(user: $u, email: $e) { alwaysNil } }";
const PRIMARY: &str =
    "mutation($u: ID!, $e: String!) { setUserEmailPrimary(user: $u, email: $e) { alwaysNil } }";

#[tokio::test]
async fn email_mutations_authz_and_primary_rules() {
    let Some(env) = setup().await else { return };
    let alice = uid(env.alice);
    let admin = uid(env.admin);

    // Another user cannot touch alice's emails; site admins can.
    env.err(
        env.alice,
        ADD,
        json!({"u": admin, "e": "x@example.com"}),
        "FORBIDDEN",
    )
    .await;
    env.ok(
        env.admin,
        ADD,
        json!({"u": alice, "e": "alice2@example.com"}),
    )
    .await;
    env.err(
        env.alice,
        ADD,
        json!({"u": alice, "e": "not an email"}),
        "INVALID_EMAIL",
    )
    .await;
    env.err(
        env.alice,
        ADD,
        json!({"u": alice, "e": "alice2@example.com"}),
        "ALREADY_EXISTS",
    )
    .await;

    let emails = env.db.user_emails().list(env.alice).await.unwrap();
    assert_eq!(emails.len(), 2);
    assert!(emails[0].is_primary && emails[0].email == "alice@example.com");
    assert!(!emails[1].is_primary && !emails[1].is_verified());

    // The primary email cannot be removed, and an unverified one cannot become primary.
    let msg = env
        .err(
            env.alice,
            REMOVE,
            json!({"u": alice, "e": "alice@example.com"}),
            "INVALID_INPUT",
        )
        .await;
    assert!(msg.contains("primary"), "{msg}");
    env.err(
        env.alice,
        PRIMARY,
        json!({"u": alice, "e": "alice2@example.com"}),
        "INVALID_INPUT",
    )
    .await;
    env.err(
        env.alice,
        PRIMARY,
        json!({"u": alice, "e": "nope@example.com"}),
        "NOT_FOUND",
    )
    .await;

    env.db
        .user_emails()
        .verify(env.alice, "alice2@example.com")
        .await
        .unwrap();
    env.ok(
        env.alice,
        PRIMARY,
        json!({"u": alice, "e": "alice2@example.com"}),
    )
    .await;
    env.ok(
        env.alice,
        REMOVE,
        json!({"u": alice, "e": "alice@example.com"}),
    )
    .await;
    let emails = env.db.user_emails().list(env.alice).await.unwrap();
    assert_eq!(emails.len(), 1);
    assert!(emails[0].is_primary && emails[0].email == "alice2@example.com");
    env.err(
        env.alice,
        REMOVE,
        json!({"u": alice, "e": "alice@example.com"}),
        "NOT_FOUND",
    )
    .await;

    // The only email cannot be removed even when it is not primary.
    env.db
        .user_emails()
        .add(env.admin, "admin2@example.com")
        .await
        .unwrap();
    sqlx::query("UPDATE user_emails SET is_primary = false WHERE user_id = $1")
        .bind(env.admin)
        .execute(env.db.pool())
        .await
        .unwrap();
    env.ok(
        env.admin,
        REMOVE,
        json!({"u": admin, "e": "admin2@example.com"}),
    )
    .await;
    let msg = env
        .err(
            env.admin,
            REMOVE,
            json!({"u": admin, "e": "admin@example.com"}),
            "INVALID_INPUT",
        )
        .await;
    assert!(msg.contains("only"), "{msg}");
}

const UPDATE_USER: &str = "mutation($u: ID!, $username: String, $name: String, $avatar: String) {
    updateUser(user: $u, username: $username, displayName: $name, avatarURL: $avatar) {
        username displayName avatarURL } }";

#[tokio::test]
async fn update_user_profile_and_username_rules() {
    let Some(env) = setup().await else { return };
    let alice = uid(env.alice);
    let admin = uid(env.admin);

    let data = env
        .ok(
            env.alice,
            UPDATE_USER,
            json!({"u": alice, "name": "  Alice A.  ", "avatar": "https://example.com/a.png"}),
        )
        .await;
    assert_eq!(
        data["updateUser"],
        json!({"username": "alice", "displayName": "Alice A.", "avatarURL": "https://example.com/a.png"})
    );
    // Omitted fields stay; empty clears.
    let data = env
        .ok(env.alice, UPDATE_USER, json!({"u": alice, "avatar": ""}))
        .await;
    assert_eq!(data["updateUser"]["displayName"], "Alice A.");
    assert_eq!(data["updateUser"]["avatarURL"], Value::Null);
    env.err(
        env.alice,
        UPDATE_USER,
        json!({"u": alice, "avatar": "javascript:alert(1)"}),
        "INVALID_INPUT",
    )
    .await;
    env.err(
        env.alice,
        UPDATE_USER,
        json!({"u": admin, "name": "Evil"}),
        "FORBIDDEN",
    )
    .await;

    // Users change their own username only when auth.enableUsernameChanges is on.
    let can = "query { currentUser { viewerCanChangeUsername } }";
    assert_eq!(
        env.ok(env.alice, can, json!({})).await["currentUser"]["viewerCanChangeUsername"],
        false
    );
    env.err(
        env.alice,
        UPDATE_USER,
        json!({"u": alice, "username": "alice2"}),
        "FORBIDDEN",
    )
    .await;
    // The same username is not a change.
    env.ok(
        env.alice,
        UPDATE_USER,
        json!({"u": alice, "username": "alice"}),
    )
    .await;
    env.set_username_changes(true);
    assert_eq!(
        env.ok(env.alice, can, json!({})).await["currentUser"]["viewerCanChangeUsername"],
        true
    );
    env.err(
        env.alice,
        UPDATE_USER,
        json!({"u": alice, "username": "admin"}),
        "ALREADY_EXISTS",
    )
    .await;
    env.err(
        env.alice,
        UPDATE_USER,
        json!({"u": alice, "username": "bad name!"}),
        "INVALID_USERNAME",
    )
    .await;
    let data = env
        .ok(
            env.alice,
            UPDATE_USER,
            json!({"u": alice, "username": "alice2"}),
        )
        .await;
    assert_eq!(data["updateUser"]["username"], "alice2");
    // The actor's cached username follows.
    let actor = env
        .auth
        .actor_for(env.alice, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actor.username, "alice2");

    // Site admins can rename anyone, whatever the setting.
    env.set_username_changes(false);
    let data = env
        .ok(
            env.admin,
            UPDATE_USER,
            json!({"u": alice, "username": "alice3"}),
        )
        .await;
    assert_eq!(data["updateUser"]["username"], "alice3");
}

const DEACTIVATE: &str = "mutation($u: ID!) { deactivateUser(user: $u) { alwaysNil } }";
const REACTIVATE: &str =
    "mutation($u: ID!) { reactivateUser(user: $u) { username deactivatedAt } }";

#[tokio::test]
async fn deactivate_and_reactivate_users() {
    let Some(env) = setup().await else { return };
    let alice = uid(env.alice);
    let admin = uid(env.admin);
    let session = env.auth.start_session(env.alice).await.unwrap();

    env.err(env.alice, DEACTIVATE, json!({"u": admin}), "FORBIDDEN")
        .await;
    env.err(env.alice, REACTIVATE, json!({"u": admin}), "FORBIDDEN")
        .await;
    env.err(env.admin, DEACTIVATE, json!({"u": admin}), "FORBIDDEN")
        .await;
    env.ok(env.admin, DEACTIVATE, json!({"u": alice})).await;
    assert!(env.auth.sessions().get(&session).await.unwrap().is_none());
    assert!(env.auth.sign_in("alice", PW, None).await.is_err());
    assert!(
        env.auth
            .actor_for(env.alice, AuthVia::Session)
            .await
            .unwrap()
            .is_none()
    );

    let list = "query($all: Boolean) { users(includeDeactivated: $all) { totalCount nodes { username deactivatedAt } } }";
    let live = env.ok(env.admin, list, json!({})).await;
    assert_eq!(live["users"]["totalCount"], 1);
    let all = env.ok(env.admin, list, json!({"all": true})).await;
    assert_eq!(all["users"]["totalCount"], 2);
    assert!(all["users"]["nodes"][1]["deactivatedAt"].is_string());

    env.err(
        env.admin,
        REACTIVATE,
        json!({"u": uid(UserId(9999))}),
        "NOT_FOUND",
    )
    .await;
    let data = env.ok(env.admin, REACTIVATE, json!({"u": alice})).await;
    assert_eq!(
        data["reactivateUser"],
        json!({"username": "alice", "deactivatedAt": null})
    );
    assert!(env.auth.sign_in("alice", PW, None).await.is_ok());

    // A reactivation fails while another live user has the username.
    env.ok(env.admin, DEACTIVATE, json!({"u": alice})).await;
    env.auth
        .sign_up("alice", "alice-new@example.com", PW)
        .await
        .unwrap();
    env.err(env.admin, REACTIVATE, json!({"u": alice}), "ALREADY_EXISTS")
        .await;
}

#[tokio::test]
async fn scim_managed_users_are_changed_through_scim_only() {
    let Some(env) = setup().await else { return };
    let written = env
        .db
        .scim()
        .write(
            None,
            &ScimWrite {
                scim_user_name: "bob".into(),
                username: "bob".into(),
                display_name: None,
                emails: vec!["bob@example.com".into()],
                previous_emails: vec![],
                account_id: "bob-1".into(),
                data: json!({}),
                active: true,
            },
        )
        .await
        .unwrap();
    let bob = uid(written.user_id);
    env.err(env.admin, DEACTIVATE, json!({"u": bob}), "FORBIDDEN")
        .await;
    env.err(
        env.admin,
        ADD,
        json!({"u": bob, "e": "bob2@example.com"}),
        "FORBIDDEN",
    )
    .await;
    env.err(
        env.admin,
        UPDATE_USER,
        json!({"u": bob, "username": "robert"}),
        "FORBIDDEN",
    )
    .await;
    // The display name and avatar are not SCIM-managed here.
    env.ok(env.admin, UPDATE_USER, json!({"u": bob, "name": "Bob"}))
        .await;
}
