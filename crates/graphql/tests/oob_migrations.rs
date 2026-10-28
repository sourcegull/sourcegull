//! Out-of-band migrations in the admin API against the test Postgres:
//! `outOfBandMigrations` with progress and errors, `node`, `setMigrationDirection` and
//! `site.upgradeReadiness.requiredOutOfBandMigrations`, site admins only.

use std::sync::Arc;

use async_graphql::Request;
use serde_json::json;
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, MigrationDescriptor, MinorVersion, NewUser, migrate};
use sourcegull_graphql::Services;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;

const LIST: &str = "{ outOfBandMigrations {
    id team component description introduced deprecated progress created lastUpdated
    nonDestructive applyReverse errors { message created }
} }";

const REQUIRED: &str = "{ site { upgradeReadiness { requiredOutOfBandMigrations {
    description progress deprecated applyReverse
} } } }";

#[tokio::test]
async fn out_of_band_migrations_admin_api() {
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
    let run = |user, query: &'static str| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            sourcegull_graphql::execute(&schema, Request::new(query), actor).await
        }
    };

    let store = db.oob_migrations();
    let current = MinorVersion::parse(sourcegull_db::upgrade::CURRENT_VERSION).unwrap();
    let next = MinorVersion::new(current.major, current.minor + 1);
    let d = |id, description, deprecated| MigrationDescriptor {
        id,
        team: "source",
        component: "encryption",
        description,
        introduced: MinorVersion::new(0, 1),
        deprecated,
        non_destructive: true,
    };
    assert!(
        store
            .register(&d(1, "re-encrypt", None), 1.0)
            .await
            .unwrap()
    );
    assert!(
        !store
            .register(&d(1, "re-encrypt secrets", None), 0.0)
            .await
            .unwrap()
    );
    assert!(
        store
            .register(&d(2, "convert widgets", Some(next)), 0.0)
            .await
            .unwrap()
    );
    store
        .update_progress(2, 0.25, &sourcegull_db::Fence::none("test"))
        .await
        .unwrap();
    store.add_error(2, "batch failed").await.unwrap();

    let resp = run(alice, LIST).await;
    assert!(!resp.errors.is_empty(), "site admins only");

    let resp = run(admin, LIST).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    let list = data["outOfBandMigrations"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["description"], "re-encrypt secrets");
    assert_eq!(list[0]["progress"], 1.0);
    assert_eq!(list[0]["introduced"], "0.1");
    assert_eq!(list[0]["deprecated"], json!(null));
    assert_eq!(list[0]["nonDestructive"], true);
    assert_eq!(list[0]["errors"], json!([]));
    assert_eq!(list[1]["progress"], 0.25);
    assert_eq!(list[1]["deprecated"], json!(next.to_string()));
    assert!(list[1]["lastUpdated"].is_string());
    assert_eq!(list[1]["errors"][0]["message"], "batch failed");
    let id = list[1]["id"].as_str().unwrap().to_owned();

    // node(id:)
    let node_query: &'static str = Box::leak(
        format!(
            r#"{{ node(id: "{id}") {{ __typename ... on OutOfBandMigration {{ description }} }} }}"#
        )
        .into_boxed_str(),
    );
    let resp = run(admin, node_query).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert_eq!(
        resp.data.into_json().unwrap(),
        json!({ "node": { "__typename": "OutOfBandMigration", "description": "convert widgets" } })
    );

    // Required before the next upgrade: only the unfinished migration deprecated in it.
    let resp = run(admin, REQUIRED).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert_eq!(
        resp.data.into_json().unwrap()["site"]["upgradeReadiness"]["requiredOutOfBandMigrations"],
        json!([{ "description": "convert widgets", "progress": 0.25,
                 "deprecated": next.to_string(), "applyReverse": false }])
    );

    // setMigrationDirection
    let set: &'static str = Box::leak(
        format!(
            r#"mutation {{ setMigrationDirection(id: "{id}", applyReverse: true) {{ alwaysNil }} }}"#
        )
        .into_boxed_str(),
    );
    let resp = run(alice, set).await;
    assert!(!resp.errors.is_empty(), "site admins only");
    assert!(!store.get(2).await.unwrap().unwrap().apply_reverse);
    let resp = run(admin, set).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert!(store.get(2).await.unwrap().unwrap().apply_reverse);
    let resp = run(
        admin,
        r#"mutation { setMigrationDirection(id: "T3V0T2ZCYW5kTWlncmF0aW9uOjk5", applyReverse: true) { alwaysNil } }"#,
    )
    .await;
    assert!(!resp.errors.is_empty(), "unknown migration");

    // Completed migrations are not required.
    store.set_direction(2, false).await.unwrap();
    store
        .update_progress(2, 1.0, &sourcegull_db::Fence::none("test"))
        .await
        .unwrap();
    let resp = run(admin, REQUIRED).await;
    assert_eq!(
        resp.data.into_json().unwrap()["site"]["upgradeReadiness"]["requiredOutOfBandMigrations"],
        json!([])
    );
}
