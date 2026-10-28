//! Temporary settings over GraphQL against the test Postgres: `temporarySettings`
//! starts empty, `overwriteTemporarySettings` replaces, `editTemporarySettings` merges top-level
//! keys, contents must be a JSON object, each user sees only their own, anonymous viewers have
//! none.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::{Services, SourcegullSchema};
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;

const GET: &str = "{ temporarySettings { contents } }";
const OVERWRITE: &str =
    "mutation($c: String!) { overwriteTemporarySettings(contents: $c) { alwaysNil } }";
const EDIT: &str =
    "mutation($c: String!) { editTemporarySettings(settingsToEdit: $c) { alwaysNil } }";

async fn run(
    schema: &SourcegullSchema,
    auth: &Authenticator,
    user: Option<UserId>,
    query: &str,
    vars: Value,
) -> Response {
    let actor = match user {
        Some(u) => auth.actor_for(u, AuthVia::Session).await.unwrap(),
        None => None,
    };
    let req = Request::new(query).variables(Variables::from_json(vars));
    sourcegull_graphql::execute(schema, req, actor).await
}

fn code(resp: &Response) -> String {
    let err = resp.errors.first().expect("an error");
    err.extensions
        .as_ref()
        .and_then(|e| e.get("code"))
        .and_then(|c| c.clone().into_json().ok())
        .and_then(|c| c.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[tokio::test]
async fn temporary_settings_round_trip() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(1, SiteConfig::default());
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(MemorySessionStore::new()),
        config.clone(),
        PasswordParams::TEST,
    );
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        let new = NewUser {
            username: name.into(),
            ..Default::default()
        };
        users.push(db.users().create(&new).await.unwrap().id);
    }
    let (alice, bob) = (users[0], users[1]);
    let schema = build_schema(Services {
        db: db.clone(),
        auth: auth.clone(),
        config: config.clone(),
        site_config_source: SiteConfigSource::Database,
        keyring: None,
        registry: Default::default(),
        repo_clients: Arc::new(sourcegull_rpc::RepoClients::new(
            Default::default(),
            Default::default(),
        )),
        authz: Checker::new(db.clone(), config),
        app: None,
    });
    let get = |user| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let resp = run(&schema, &auth, Some(user), GET, json!({})).await;
            assert!(resp.errors.is_empty(), "{:?}", resp.errors);
            let data = resp.data.into_json().unwrap();
            serde_json::from_str::<Value>(data["temporarySettings"]["contents"].as_str().unwrap())
                .unwrap()
        }
    };
    let ok = |user, q: &'static str, c: Value| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let vars = json!({ "c": c.to_string() });
            let resp = run(&schema, &auth, Some(user), q, vars).await;
            assert!(resp.errors.is_empty(), "{q}: {:?}", resp.errors);
        }
    };

    assert_eq!(get(alice).await, json!({}));

    let recent = json!([{
        "query": "repo:acme refund",
        "resultCount": 12,
        "limitHit": false,
        "timestamp": "2026-10-22T10:00:00Z",
    }]);
    ok(
        alice,
        OVERWRITE,
        json!({ "search.input.recentSearches": recent, "a": 1 }),
    )
    .await;
    assert_eq!(
        get(alice).await,
        json!({ "search.input.recentSearches": recent, "a": 1 })
    );
    assert_eq!(get(bob).await, json!({}), "per user");

    // Edit merges top-level keys and keeps the others.
    ok(alice, EDIT, json!({ "a": 2, "b": true })).await;
    assert_eq!(
        get(alice).await,
        json!({ "search.input.recentSearches": recent, "a": 2, "b": true })
    );
    // Overwrite replaces everything.
    ok(alice, OVERWRITE, json!({ "b": false })).await;
    assert_eq!(get(alice).await, json!({ "b": false }));
    // Edit creates the row when none exists.
    ok(bob, EDIT, json!({ "x": [1] })).await;
    assert_eq!(get(bob).await, json!({ "x": [1] }));

    // Not an object.
    for (q, c) in [(OVERWRITE, "[1]"), (EDIT, "nope"), (EDIT, "3")] {
        let resp = run(&schema, &auth, Some(alice), q, json!({ "c": c })).await;
        assert_eq!(code(&resp), "INVALID_INPUT", "{q} {c}");
    }
    // Too large.
    let big = json!({ "k": "x".repeat(600 * 1024) }).to_string();
    let resp = run(&schema, &auth, Some(alice), OVERWRITE, json!({ "c": big })).await;
    assert_eq!(code(&resp), "INVALID_INPUT");

    // Anonymous viewers have no temporary settings.
    let resp = run(&schema, &auth, None, GET, json!({})).await;
    assert_eq!(code(&resp), "UNAUTHORIZED");
    let resp = run(&schema, &auth, None, EDIT, json!({ "c": "{}" })).await;
    assert_eq!(code(&resp), "UNAUTHORIZED");
    assert_eq!(get(alice).await, json!({ "b": false }));
}
