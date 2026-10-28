//! Ranking over GraphQL against the test Postgres: `rankingSummary` from stored
//! runs and ranks, `bumpDerivativeGraphKey`, and site-admin-only access.

use std::sync::Arc;

use async_graphql::Request;
use serde_json::json;
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::Services;
use sourcegull_ranking::store::{RankingStore, RepoRanksPayload, RunStats};
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sourcegull_types::RepoId;

const SUMMARY: &str = "{ rankingSummary {
    derivativeGraphKey enabled numExportedIndexes numRankedRepositories numRankedPaths
    rankingSummary { graphKey visibleToSearch completedAt failureMessage numExportedIndexes
                     numPaths numEdges iterations numRepositoriesChanged }
} }";

#[tokio::test]
async fn ranking_summary() {
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
    let repo: i32 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
         VALUES ('gitlab.example.com/a/b', '1', 'gitlab', 'https://gitlab.example.com/')
         RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let store = RankingStore::new(&db);
    let failed = store.start_run("x", 1).await.unwrap();
    store.fail_run(failed, "upload 3: gone").await.unwrap();
    let run = store.start_run("y", 2).await.unwrap();
    let paths = [("a.go".to_owned(), 1.0), ("b.go".to_owned(), 0.5)];
    let payload = RepoRanksPayload {
        repo: RepoId(repo),
        num_paths: 2,
        payload: sourcegull_types::ranking::encode_ranks(&paths),
    };
    let stats = RunStats {
        num_paths: 3,
        num_edges: 2,
        iterations: 9,
    };
    assert_eq!(
        store
            .finish_run(run, stats, &[payload], &sourcegull_db::Fence::none("test"))
            .await
            .unwrap(),
        1
    );

    let run_query = |user, query: &'static str| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            sourcegull_graphql::execute(&schema, Request::new(query), actor).await
        }
    };
    let resp = run_query(alice, SUMMARY).await;
    assert!(!resp.errors.is_empty(), "non-admins are refused");
    let resp = run_query(admin, SUMMARY).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let mut data = resp.data.into_json().unwrap();
    let s = &mut data["rankingSummary"];
    assert!(s["rankingSummary"][0]["completedAt"].is_string());
    s["rankingSummary"][0]["completedAt"] = json!("t");
    s["rankingSummary"][1]["completedAt"] = json!("t");
    assert_eq!(
        *s,
        json!({
            "derivativeGraphKey": run.to_string(),
            "enabled": true,
            "numExportedIndexes": 2,
            "numRankedRepositories": 1,
            "numRankedPaths": 2,
            "rankingSummary": [
                {"graphKey": run.to_string(), "visibleToSearch": true, "completedAt": "t",
                 "failureMessage": null, "numExportedIndexes": 2, "numPaths": 3, "numEdges": 2,
                 "iterations": 9, "numRepositoriesChanged": 1},
                {"graphKey": failed.to_string(), "visibleToSearch": false, "completedAt": "t",
                 "failureMessage": "upload 3: gone", "numExportedIndexes": 1, "numPaths": null,
                 "numEdges": null, "iterations": null, "numRepositoriesChanged": null},
            ],
        })
    );

    // A bump makes the next pass rerun: the completed run no longer matches any inputs.
    const BUMP: &str = "mutation { bumpDerivativeGraphKey { alwaysNil } }";
    assert!(!run_query(alice, BUMP).await.errors.is_empty());
    let resp = run_query(admin, BUMP).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    assert_eq!(store.last_completed().await.unwrap().unwrap().inputs, "");
}
