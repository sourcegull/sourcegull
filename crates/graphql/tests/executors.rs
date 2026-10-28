//! Executors over GraphQL against the test Postgres: the list of executors from their
//! heartbeats, filters and site-admin-only access.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::json;
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{ActiveJob, Db, ExecutorInfo, NewUser, migrate};
use sourcegull_graphql::Services;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;

const LIST: &str = "query($query: String, $active: Boolean) {
    areExecutorsConfigured
    executors(query: $query, active: $active) {
        totalCount
        nodes { hostname queueName queueNames active os executorVersion runtime dockerVersion
                gitVersion compatibility activeJobs { queue id } }
    }
}";

#[tokio::test]
async fn executors_from_heartbeats() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            executors_access_token: Some("executor-secret-0123456789".into()),
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

    let executors = db.executors();
    let info = |name: &str, version: &str| ExecutorInfo {
        hostname: name.into(),
        queue_names: vec!["codeintel".into()],
        os: "linux".into(),
        architecture: "x86_64".into(),
        executor_version: version.into(),
        runtime: "docker".into(),
        runtime_version: "27.3.1".into(),
        git_version: "2.47.0".into(),
    };
    executors
        .heartbeat(
            &info("exec-a", env!("CARGO_PKG_VERSION")),
            &[ActiveJob {
                queue: "codeintel".into(),
                id: 7,
            }],
        )
        .await
        .unwrap();
    executors
        .heartbeat(&info("exec-b", "0.9.0"), &[])
        .await
        .unwrap();
    executors
        .heartbeat(&info("exec-old", "dev"), &[])
        .await
        .unwrap();
    sourcegull_db::sqlx::query(
        "UPDATE executor_heartbeats SET last_seen_at = now() - interval '10 minutes'
         WHERE hostname = 'exec-old'",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let run = |user, vars: serde_json::Value| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            let req = Request::new(LIST).variables(Variables::from_json(vars));
            sourcegull_graphql::execute(&schema, req, actor).await
        }
    };
    let resp = run(alice, json!({})).await;
    assert!(!resp.errors.is_empty(), "site admins only");

    let resp = run(admin, json!({})).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["areExecutorsConfigured"], true);
    let list = &data["executors"];
    assert_eq!(list["totalCount"], 3);
    let node = |name: &str| {
        list["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["hostname"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(
        list["nodes"][2]["hostname"], "exec-old",
        "most recently seen first"
    );
    let a = node("exec-a");
    assert_eq!(a["queueName"], "codeintel");
    assert_eq!(a["active"], true);
    assert_eq!(a["dockerVersion"], "27.3.1");
    assert_eq!(a["compatibility"], "UP_TO_DATE");
    assert_eq!(a["activeJobs"], json!([{"queue": "codeintel", "id": 7}]));
    let old = node("exec-old");
    assert_eq!(old["active"], false);
    assert_eq!(old["compatibility"], serde_json::Value::Null);

    let resp = run(admin, json!({"active": true, "query": "EXEC-B"})).await;
    let data = resp.data.into_json().unwrap();
    assert_eq!(data["executors"]["totalCount"], 1);
    assert_eq!(data["executors"]["nodes"][0]["compatibility"], "TOO_NEW");
}
