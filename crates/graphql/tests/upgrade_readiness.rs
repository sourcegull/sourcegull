//! `site { upgradeReadiness }` against the test Postgres: no drift on a migrated
//! database, drift with fix statements after a manual change, site admins only.

use std::sync::Arc;

use async_graphql::Request;
use serde_json::json;
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::Services;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;

const QUERY: &str = "{ site { upgradeReadiness {
    schemaVersion schemaDriftNote
    schemaDrift { name problem solution diff statements urlHint }
} } }";

#[tokio::test]
async fn upgrade_readiness_reports_schema_drift() {
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
    let run = |user| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            sourcegull_graphql::execute(&schema, Request::new(QUERY), actor).await
        }
    };

    let resp = run(alice).await;
    assert!(!resp.errors.is_empty(), "site admins only");

    let resp = run(admin).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert_eq!(
        resp.data.into_json().unwrap(),
        json!({ "site": { "upgradeReadiness": {
            "schemaVersion": sourcegull_db::upgrade::CURRENT_VERSION,
            "schemaDriftNote": null,
            "schemaDrift": [],
        } } })
    );

    sqlx::raw_sql(
        "ALTER TABLE repo ALTER COLUMN stars TYPE bigint; CREATE TABLE drift_scratch (id int)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let resp = run(admin).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(
        data["site"]["upgradeReadiness"]["schemaDrift"],
        json!([
            {
                "name": "column \"repo.stars\"",
                "problem": "type differs",
                "solution": "Change it to the expected definition with the statements below.",
                "diff": "- bigint\n+ integer",
                "statements": ["ALTER TABLE repo ALTER COLUMN stars TYPE integer USING stars::integer;"],
                "urlHint": null,
            },
            {
                "name": "table \"drift_scratch\"",
                "problem": "unexpected",
                "solution": "Drop it with the statements below, unless it was added on purpose.",
                "diff": null,
                "statements": ["DROP TABLE drift_scratch;"],
                "urlHint": null,
            },
        ])
    );
}
