//! Organizations, invitations, settings and teams synced from GitLab groups over GraphQL
//! against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`): membership and the
//! permission rules (members administer, site admins list and delete),
//! invitations by username and by email (link tokens, accept/decline, revoke), the settings
//! cascade (global → org → user) with `settingsMutation`, and `syncTeamsFromGitLabGroups`
//! against the mock GitLab.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_codehost_gitlab::testing::{MockGitLab, MockGroup, MockUser};
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, NewExternalService, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_services::relay::{marshal_id, marshal_string_id};
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceKind, UserId};

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    auth: Authenticator,
    schema: SourcegullSchema,
    admin: UserId,
    alice: UserId,
    bob: UserId,
    carol: UserId,
}

async fn setup() -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            external_url: Some("https://sourcegull.test".into()),
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
        .sign_up("admin", "admin@example.com", "correct horse battery")
        .await
        .unwrap()
        .id;
    let mut users = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let id = db
            .users()
            .create(&NewUser {
                username: name.into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .id;
        let email = format!("{name}@example.com");
        db.user_emails().add(id, &email).await.unwrap();
        db.user_emails().verify(id, &email).await.unwrap();
        users.push(id);
    }
    let authz = sourcegull_authz::Checker::new(db.clone(), config.clone());
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config,
        site_config_source: SiteConfigSource::Database,
        keyring: Some(keyring.clone()),
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz,
        app: None,
    });
    Some(Env {
        _t: t,
        db,
        keyring,
        auth,
        schema,
        admin,
        alice: users[0],
        bob: users[1],
        carol: users[2],
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

    async fn ok(&self, user: UserId, query: &str, vars: Value) -> Value {
        let resp = self.run(user, query, vars).await;
        assert!(resp.errors.is_empty(), "{query}: {:?}", resp.errors);
        resp.data.into_json().unwrap()
    }

    async fn error_code(&self, user: UserId, query: &str, vars: Value) -> String {
        let resp = self.run(user, query, vars).await;
        let e = resp
            .errors
            .first()
            .unwrap_or_else(|| panic!("{query}: no error"));
        e.extensions
            .as_ref()
            .and_then(|x| x.get("code"))
            .map(|c| c.to_string().trim_matches('"').to_owned())
            .unwrap_or_default()
    }

    /// Creates an org as `user`, returning its ID.
    async fn create_org(&self, user: UserId, name: &str) -> String {
        let d = self
            .ok(
                user,
                "mutation($n: String!) { createOrganization(name: $n, displayName: \"Acme\") {
                    id name displayName url viewerIsMember viewerCanAdminister } }",
                json!({ "n": name }),
            )
            .await;
        let org = &d["createOrganization"];
        assert_eq!(org["viewerIsMember"], true);
        assert_eq!(org["viewerCanAdminister"], true);
        assert_eq!(org["url"], format!("/organizations/{name}"));
        org["id"].as_str().unwrap().to_owned()
    }
}

fn user_id(u: UserId) -> String {
    marshal_id("User", u.get())
}

const MEMBERS: &str = r#"query($n: String!, $q: String) { organization(name: $n) {
    viewerIsMember members(query: $q) { totalCount nodes { username } } } }"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn organizations_members_and_permissions() {
    let Some(env) = setup().await else { return };
    let org = env.create_org(env.alice, "acme").await;
    // Names are shared with users and teams.
    let create = "mutation($n: String!) { createOrganization(name: $n) { id } }";
    for taken in ["ACME", "bob"] {
        assert_eq!(
            env.error_code(env.alice, create, json!({ "n": taken }))
                .await,
            "ALREADY_EXISTS"
        );
    }

    // Listing every organization is for site admins.
    let list = r#"query($q: String) { organizations(query: $q) { totalCount nodes { name } } }"#;
    assert_eq!(
        env.error_code(env.alice, list, json!({})).await,
        "FORBIDDEN"
    );
    env.create_org(env.carol, "zeta").await;
    let d = env.ok(env.admin, list, json!({ "q": "zet" })).await;
    assert_eq!(d["organizations"]["totalCount"], 1);
    assert_eq!(d["organizations"]["nodes"][0]["name"], "zeta");
    // Display names match too.
    let d = env.ok(env.admin, list, json!({ "q": "ACME" })).await;
    assert_eq!(d["organizations"]["totalCount"], 2);

    // Members are visible to members and site admins only.
    assert_eq!(
        env.error_code(env.bob, MEMBERS, json!({ "n": "acme" }))
            .await,
        "FORBIDDEN"
    );
    let d = env
        .ok(
            env.bob,
            "{ organization(name: \"acme\") { viewerIsMember viewerCanAdminister } }",
            json!({}),
        )
        .await;
    assert_eq!(d["organization"]["viewerIsMember"], false);
    assert_eq!(d["organization"]["viewerCanAdminister"], false);

    // Only members update.
    let update = "mutation($id: ID!, $d: String) { updateOrganization(id: $id, displayName: $d) {
        displayName } }";
    assert_eq!(
        env.error_code(env.bob, update, json!({ "id": org, "d": "x" }))
            .await,
        "FORBIDDEN"
    );
    let d = env
        .ok(env.alice, update, json!({ "id": org, "d": "" }))
        .await;
    assert_eq!(d["updateOrganization"]["displayName"], Value::Null);

    // Adding without an invitation is for site admins.
    let add = "mutation($o: ID!, $u: String!) { addUserToOrganization(organization: $o,
        username: $u) { alwaysNil } }";
    assert_eq!(
        env.error_code(env.alice, add, json!({ "o": org, "u": "bob" }))
            .await,
        "FORBIDDEN"
    );
    env.ok(env.admin, add, json!({ "o": org, "u": "bob" }))
        .await;
    assert_eq!(
        env.error_code(env.admin, add, json!({ "o": org, "u": "bob" }))
            .await,
        "ALREADY_EXISTS"
    );
    let d = env.ok(env.admin, MEMBERS, json!({ "n": "acme" })).await;
    assert_eq!(d["organization"]["members"]["totalCount"], 2);
    let d = env
        .ok(env.bob, MEMBERS, json!({ "n": "acme", "q": "AL" }))
        .await;
    assert_eq!(d["organization"]["viewerIsMember"], true);
    assert_eq!(
        d["organization"]["members"]["nodes"],
        json!([{ "username": "alice" }])
    );
    let d = env
        .ok(
            env.bob,
            "{ currentUser { organizations { nodes { name } } } }",
            json!({}),
        )
        .await;
    assert_eq!(
        d["currentUser"]["organizations"]["nodes"],
        json!([{ "name": "acme" }])
    );

    // Members (not others) remove members.
    let remove = "mutation($o: ID!, $u: ID!) { removeUserFromOrganization(organization: $o,
        user: $u) { alwaysNil } }";
    assert_eq!(
        env.error_code(
            env.carol,
            remove,
            json!({ "o": org, "u": user_id(env.bob) })
        )
        .await,
        "FORBIDDEN"
    );
    env.ok(
        env.alice,
        remove,
        json!({ "o": org, "u": user_id(env.bob) }),
    )
    .await;
    let d = env.ok(env.alice, MEMBERS, json!({ "n": "acme" })).await;
    assert_eq!(d["organization"]["members"]["totalCount"], 1);

    // Deleting is for site admins; the name becomes free.
    let delete = "mutation($o: ID!) { deleteOrganization(organization: $o) { alwaysNil } }";
    assert_eq!(
        env.error_code(env.alice, delete, json!({ "o": org })).await,
        "FORBIDDEN"
    );
    env.ok(env.admin, delete, json!({ "o": org })).await;
    let d = env
        .ok(
            env.alice,
            "{ organization(name: \"acme\") { id } }",
            json!({}),
        )
        .await;
    assert_eq!(d["organization"], Value::Null);
    env.create_org(env.bob, "acme").await;
}

const INVITE: &str = r#"mutation($o: ID!, $u: String, $e: String) {
    inviteUserToOrganization(organization: $o, username: $u, email: $e) {
        sentInvitationEmail invitationURL } }"#;
const RESPOND: &str = r#"mutation($i: ID!, $r: OrganizationInvitationResponseType!) {
    respondToOrganizationInvitation(organizationInvitation: $i, responseType: $r) { alwaysNil } }"#;
const MINE: &str = r#"{ viewerOrganizationInvitations { id recipientEmail isVerifiedEmail
    organization { name } sender { username } } }"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invitations() {
    let Some(env) = setup().await else { return };
    let org = env.create_org(env.alice, "acme").await;

    // By username: a link with a token, no email without SMTP.
    let d = env
        .ok(env.alice, INVITE, json!({ "o": org, "u": "bob" }))
        .await;
    let res = &d["inviteUserToOrganization"];
    assert_eq!(res["sentInvitationEmail"], false);
    let url = res["invitationURL"].as_str().unwrap().to_owned();
    let token = url
        .strip_prefix("https://sourcegull.test/organizations/invitation/")
        .unwrap_or_else(|| panic!("{url}"))
        .to_owned();
    assert_eq!(
        env.error_code(env.alice, INVITE, json!({ "o": org, "u": "alice" }))
            .await,
        "ALREADY_EXISTS"
    );
    assert_eq!(
        env.error_code(env.carol, INVITE, json!({ "o": org, "u": "carol" }))
            .await,
        "FORBIDDEN"
    );
    assert_eq!(
        env.error_code(env.alice, INVITE, json!({ "o": org })).await,
        "INVALID_INPUT"
    );

    // The pending list is for members; the recipient sees their own.
    let pending = r#"query($o: ID!) { pendingInvitations(org: $o) { id recipient { username }
        expiresAt respondURL } }"#;
    assert_eq!(
        env.error_code(env.carol, pending, json!({ "o": org }))
            .await,
        "FORBIDDEN"
    );
    let d = env.ok(env.alice, pending, json!({ "o": org })).await;
    let list = d["pendingInvitations"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["recipient"]["username"], "bob");
    assert_eq!(list[0]["respondURL"], "/organizations/acme");
    let inv = list[0]["id"].as_str().unwrap().to_owned();
    let d = env.ok(env.bob, MINE, json!({})).await;
    assert_eq!(d["viewerOrganizationInvitations"][0]["id"], inv);
    assert_eq!(
        d["viewerOrganizationInvitations"][0]["sender"]["username"],
        "alice"
    );
    let d = env
        .ok(
            env.bob,
            "{ organization(name: \"acme\") { viewerPendingInvitation { id } } }",
            json!({}),
        )
        .await;
    assert_eq!(d["organization"]["viewerPendingInvitation"]["id"], inv);

    // Others cannot see or answer it, by ID or by token.
    let node = "query($i: ID!) { node(id: $i) { id } }";
    let d = env.ok(env.carol, node, json!({ "i": inv })).await;
    assert_eq!(d["node"], Value::Null);
    assert_eq!(
        env.error_code(env.carol, RESPOND, json!({ "i": inv, "r": "ACCEPT" }))
            .await,
        "NOT_FOUND"
    );
    let by_token =
        "query($t: String!) { invitationByToken(token: $t) { id organization { name } } }";
    assert_eq!(
        env.error_code(env.carol, by_token, json!({ "t": token }))
            .await,
        "NOT_FOUND"
    );
    assert_eq!(
        env.error_code(env.bob, by_token, json!({ "t": "nope" }))
            .await,
        "NOT_FOUND"
    );
    let d = env.ok(env.bob, by_token, json!({ "t": token })).await;
    assert_eq!(d["invitationByToken"]["id"], inv);

    // Declining does not join; an answered invitation cannot be answered again.
    env.ok(env.bob, RESPOND, json!({ "i": inv, "r": "REJECT" }))
        .await;
    assert_eq!(
        env.error_code(env.bob, RESPOND, json!({ "i": inv, "r": "ACCEPT" }))
            .await,
        "CONFLICT"
    );
    let d = env
        .ok(
            env.bob,
            "{ organization(name: \"acme\") { viewerIsMember } }",
            json!({}),
        )
        .await;
    assert_eq!(d["organization"]["viewerIsMember"], false);

    // By email: a verified address of a user invites that user.
    env.ok(
        env.alice,
        INVITE,
        json!({ "o": org, "e": "carol@example.com" }),
    )
    .await;
    let d = env.ok(env.carol, MINE, json!({})).await;
    let carol_inv = d["viewerOrganizationInvitations"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        d["viewerOrganizationInvitations"][0]["recipientEmail"],
        Value::Null
    );
    // Revoked invitations disappear; revoking twice conflicts.
    let revoke = "mutation($i: ID!) { revokeOrganizationInvitation(organizationInvitation: $i) {
        alwaysNil } }";
    assert_eq!(
        env.error_code(env.bob, revoke, json!({ "i": carol_inv }))
            .await,
        "NOT_FOUND"
    );
    env.ok(env.alice, revoke, json!({ "i": carol_inv })).await;
    assert_eq!(
        env.error_code(env.alice, revoke, json!({ "i": carol_inv }))
            .await,
        "CONFLICT"
    );
    let d = env.ok(env.carol, MINE, json!({})).await;
    assert_eq!(d["viewerOrganizationInvitations"], json!([]));

    // An address nobody has yet: whoever verifies it later can accept.
    env.ok(
        env.alice,
        INVITE,
        json!({ "o": org, "e": "dave@example.com" }),
    )
    .await;
    let dave = env
        .db
        .users()
        .create(&NewUser {
            username: "dave".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    env.db
        .user_emails()
        .add(dave, "Dave@Example.com")
        .await
        .unwrap();
    let d = env.ok(dave, MINE, json!({})).await;
    assert_eq!(d["viewerOrganizationInvitations"], json!([]));
    env.db
        .user_emails()
        .verify(dave, "Dave@Example.com")
        .await
        .unwrap();
    let d = env.ok(dave, MINE, json!({})).await;
    let mine = &d["viewerOrganizationInvitations"][0];
    assert_eq!(mine["recipientEmail"], "dave@example.com");
    assert_eq!(mine["isVerifiedEmail"], true);
    let dave_inv = mine["id"].as_str().unwrap().to_owned();
    // Resending needs SMTP.
    let resend = "mutation($i: ID!) { resendOrganizationInvitationNotification(
        organizationInvitation: $i) { alwaysNil } }";
    assert_eq!(
        env.error_code(env.alice, resend, json!({ "i": dave_inv }))
            .await,
        "UNAVAILABLE"
    );
    env.ok(dave, RESPOND, json!({ "i": dave_inv, "r": "ACCEPT" }))
        .await;
    let d = env.ok(dave, MEMBERS, json!({ "n": "acme" })).await;
    assert_eq!(d["organization"]["viewerIsMember"], true);
    assert_eq!(d["organization"]["members"]["totalCount"], 2);
}

const SET: &str = r#"mutation($s: ID!, $l: Int, $c: String!) {
    settingsMutation(input: { subject: $s, lastID: $l }) {
        overwriteSettings(contents: $c) { empty { alwaysNil } } } }"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_cascade() {
    let Some(env) = setup().await else { return };
    let org = env.create_org(env.alice, "acme").await;
    let site = marshal_string_id("Site", "site");

    // Global settings: site admins write, everyone reads.
    let global = r#"{"search.includeArchived": true, "search.includeForks": false}"#;
    assert_eq!(
        env.error_code(env.alice, SET, json!({ "s": site, "c": global }))
            .await,
        "FORBIDDEN"
    );
    env.ok(env.admin, SET, json!({ "s": site, "c": global }))
        .await;

    // Org settings: members write and read; others neither.
    let org_settings = "// org\n{\"search.includeForks\": true,}";
    assert_eq!(
        env.error_code(env.bob, SET, json!({ "s": org, "c": org_settings }))
            .await,
        "FORBIDDEN"
    );
    env.ok(env.alice, SET, json!({ "s": org, "c": org_settings }))
        .await;
    // A stale lastID conflicts.
    assert_eq!(
        env.error_code(env.alice, SET, json!({ "s": org, "c": "{}" }))
            .await,
        "CONFLICT"
    );
    for bad in ["[1]", "{\"search.includeForks\": \"yes\"}", "{"] {
        assert_eq!(
            env.error_code(env.alice, SET, json!({ "s": org, "c": bad }))
                .await,
            "VALIDATION",
            "{bad}"
        );
    }
    let latest = r#"{ organization(name: "acme") { latestSettings { id contents
        author { username } subject { __typename } }
        settingsCascade { subjects { __typename settingsURL } final } } }"#;
    assert_eq!(
        env.error_code(env.bob, latest, json!({})).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.alice, latest, json!({})).await;
    let o = &d["organization"];
    assert_eq!(o["latestSettings"]["contents"], org_settings);
    assert_eq!(o["latestSettings"]["author"]["username"], "alice");
    assert_eq!(o["latestSettings"]["subject"]["__typename"], "Org");
    assert_eq!(
        o["settingsCascade"]["subjects"],
        json!([
            { "__typename": "Site", "settingsURL": "/site-admin/global-settings" },
            { "__typename": "Org", "settingsURL": "/organizations/acme/settings" },
        ])
    );
    let merged: Value =
        serde_json::from_str(o["settingsCascade"]["final"].as_str().unwrap()).unwrap();
    assert_eq!(
        merged,
        json!({ "search.includeArchived": true, "search.includeForks": true })
    );
    let last_id = o["latestSettings"]["id"].as_i64().unwrap();
    env.ok(
        env.alice,
        SET,
        json!({ "s": org, "l": last_id, "c": "{\"search.includeArchived\": false}" }),
    )
    .await;

    // User settings: the user writes their own; the cascade is site, orgs, user.
    let bob = user_id(env.bob);
    assert_eq!(
        env.error_code(env.alice, SET, json!({ "s": bob, "c": "{}" }))
            .await,
        "FORBIDDEN"
    );
    env.ok(
        env.alice,
        SET,
        json!({ "s": user_id(env.alice), "c": "{\"search.defaultPatternType\": \"regexp\"}" }),
    )
    .await;
    let d = env
        .ok(
            env.alice,
            "{ viewerSettings { subjects { __typename } final } }",
            json!({}),
        )
        .await;
    assert_eq!(
        d["viewerSettings"]["subjects"],
        json!([{ "__typename": "Site" }, { "__typename": "Org" }, { "__typename": "User" }])
    );
    let merged: Value =
        serde_json::from_str(d["viewerSettings"]["final"].as_str().unwrap()).unwrap();
    assert_eq!(
        merged,
        json!({
            "search.includeArchived": false,
            "search.includeForks": false,
            "search.defaultPatternType": "regexp",
        })
    );
    // Another user's settings are private.
    let other = r#"query($u: String!) { user(username: $u) { latestSettings { id } } }"#;
    assert_eq!(
        env.error_code(env.alice, other, json!({ "u": "bob" }))
            .await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, other, json!({ "u": "alice" })).await;
    assert!(d["user"]["latestSettings"]["id"].is_i64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_synced_from_gitlab_groups() {
    let Some(env) = setup().await else { return };
    let gitlab = MockGitLab::start("admin-token").await;
    gitlab.with(|s| {
        s.group_tree = vec![
            MockGroup::new(1, "acme", None),
            MockGroup::new(2, "acme/backend", Some(1)),
            MockGroup::new(3, "acme/backend/db", Some(2)),
            MockGroup::new(4, "tools", None),
            MockGroup::new(5, "taken", None),
        ];
        let mut ali = MockUser::new(7, "ali-gl");
        ali.groups = vec!["acme".into(), "acme/backend".into()];
        let mut bo = MockUser::new(8, "bo-gl");
        bo.groups = vec!["acme/backend/db".into(), "tools".into()];
        let mut stranger = MockUser::new(9, "stranger");
        stranger.groups = vec!["acme".into()];
        s.users = vec![ali, bo, stranger];
    });
    let service_id = format!("{}/", gitlab.url);
    for (user, account) in [(env.alice, "7"), (env.bob, "8")] {
        sourcegull_db::sqlx::query(
            "INSERT INTO user_external_accounts (user_id, service_type, service_id, account_id) \
             VALUES ($1, 'gitlab', $2, $3)",
        )
        .bind(user.get())
        .bind(&service_id)
        .bind(account)
        .execute(env.db.pool())
        .await
        .unwrap();
    }
    let svc = env
        .db
        .external_services(Some(env.keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config: format!(
                r#"{{"url": "{}", "token": "admin-token", "projectQuery": ["none"]}}"#,
                gitlab.url
            ),
        })
        .await
        .unwrap();
    // A team made in Sourcegull with a group's name is adopted; a name taken by an org is not.
    env.ok(
        env.carol,
        "mutation { createTeam(name: \"tools\") { id } }",
        json!({}),
    )
    .await;
    env.create_org(env.carol, "taken").await;

    let sync = "mutation { syncTeamsFromGitLabGroups { created updated deleted skipped members
        warnings } }";
    assert_eq!(
        env.error_code(env.alice, sync, json!({})).await,
        "FORBIDDEN"
    );
    let d = env.ok(env.admin, sync, json!({})).await;
    let s = &d["syncTeamsFromGitLabGroups"];
    assert_eq!(s["created"], 4, "{s}");
    assert_eq!(s["skipped"], 1, "{s}");
    assert_eq!(s["deleted"], 0, "{s}");
    // alice in acme and acme/backend, bob in acme/backend/db and tools.
    assert_eq!(s["members"], 4, "{s}");
    assert!(
        s["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("taken"))
    );

    let team = r#"query($n: String!) { team(name: $n) { name displayName readonly
        syncedFromCodeHost parentTeam { name } childTeams { nodes { name } }
        members { nodes { ... on User { username } } } } }"#;
    let d = env
        .ok(env.alice, team, json!({ "n": "acme/backend" }))
        .await;
    assert_eq!(
        d["team"],
        json!({
            "name": "acme/backend",
            "displayName": "backend",
            "readonly": true,
            "syncedFromCodeHost": true,
            "parentTeam": { "name": "acme" },
            "childTeams": { "nodes": [{ "name": "acme/backend/db" }] },
            "members": { "nodes": [{ "username": "alice" }] },
        })
    );
    let d = env.ok(env.alice, team, json!({ "n": "tools" })).await;
    assert_eq!(d["team"]["syncedFromCodeHost"], true);
    assert_eq!(
        d["team"]["members"]["nodes"],
        json!([{ "username": "bob" }])
    );
    // Synced teams are read-only for their members.
    assert_eq!(
        env.error_code(
            env.alice,
            "mutation { addTeamMembers(teamName: \"acme/backend\", members: [{ username: \"carol\" }]) { id } }",
            json!({}),
        )
        .await,
        "FORBIDDEN"
    );

    // Renamed, moved and removed groups; bob joins the renamed one.
    gitlab.with(|s| {
        s.group_tree.retain(|g| g.id != 3);
        s.group_tree[1] = MockGroup::new(2, "acme/server", Some(1));
        for u in &mut s.users {
            u.groups = u
                .groups
                .iter()
                .map(|g| {
                    g.replace("acme/backend/db", "acme/server")
                        .replace("acme/backend", "acme/server")
                })
                .collect();
        }
    });
    let d = env.ok(env.admin, sync, json!({})).await;
    let s = &d["syncTeamsFromGitLabGroups"];
    assert_eq!(
        (
            s["created"].clone(),
            s["updated"].clone(),
            s["deleted"].clone()
        ),
        (json!(0), json!(1), json!(1)),
        "{s}"
    );
    let d = env.ok(env.alice, team, json!({ "n": "acme/server" })).await;
    assert_eq!(
        d["team"]["members"]["nodes"],
        json!([{ "username": "alice" }, { "username": "bob" }])
    );
    assert_eq!(d["team"]["childTeams"]["nodes"], json!([]));
    let d = env
        .ok(env.alice, team, json!({ "n": "acme/backend" }))
        .await;
    assert_eq!(d["team"], Value::Null);

    // A deleted connection takes its teams along.
    env.db
        .external_services(Some(env.keyring.clone()))
        .soft_delete(svc.id)
        .await
        .unwrap();
    let d = env.ok(env.admin, sync, json!({})).await;
    assert_eq!(d["syncTeamsFromGitLabGroups"]["deleted"], 3);
    let d = env
        .ok(
            env.alice,
            "{ teams(includeChildTeams: true) { totalCount } }",
            json!({}),
        )
        .await;
    assert_eq!(d["teams"]["totalCount"], 0);
}
