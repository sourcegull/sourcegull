//! Precise index coverage over GraphQL against the test Postgres: each coverage state from
//! stored uploads, index jobs and policies, filters and paging, and site-admin-only access.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::json;
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_codeintel_policies::store::CoverageStore;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{Db, NewUser, migrate};
use sourcegull_graphql::Services;
use sourcegull_services::SiteConfigSource;
use sourcegull_testutil::TestDb;
use sqlx::PgPool;

const COVERAGE: &str = "query($state: PreciseIndexCoverageState, $query: String, $first: Int!,
                              $after: String) {
    preciseIndexCoverage(staleAfterDays: 7) {
        staleAfterDays autoIndexingEnabled
        counts { upToDate stale failing none notTargeted total }
        repositories(state: $state, query: $query, first: $first, after: $after) {
            totalCount
            pageInfo { hasNextPage endCursor }
            nodes { repositoryName codeGraphURL state headCommit indexedCommit indexedAtHead
                    failureMessage failureSource }
        }
    }
}";

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

async fn repo(pool: &PgPool, name: &str, head: Option<&str>) -> i32 {
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id)
         VALUES ($1, $1, 'gitlab', 'https://gitlab.example.com/') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gitserver_repos (repo_id, clone_status, head_oid) VALUES ($1, 'cloned', $2)
         ON CONFLICT (repo_id) DO UPDATE SET clone_status = 'cloned', head_oid = $2",
    )
    .bind(id)
    .bind(head)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn upload(pool: &PgPool, repo: i32, commit: &str, state: &str, days_ago: i32) {
    sqlx::query(
        "INSERT INTO codeintel_uploads (repository_id, commit, state, failure_message,
                                        uploaded_at, finished_at)
         VALUES ($1, $2, $3, CASE WHEN $3 = 'failed' THEN 'bad index' END,
                 now() - make_interval(days => $4), now() - make_interval(days => $4))",
    )
    .bind(repo)
    .bind(commit)
    .bind(state)
    .bind(days_ago)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn precise_index_coverage() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let site = SiteConfig {
        code_intel_auto_indexing_enabled: true,
        ..SiteConfig::default()
    };
    let config = ConfigHandle::new(1, site);
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

    let pool = db.pool();
    // At the tip.
    let a = repo(pool, "g/a", Some(&sha('a'))).await;
    upload(pool, a, &sha('1'), "completed", 40).await;
    upload(pool, a, &sha('a'), "completed", 30).await;
    // Behind the tip and older than the window.
    let b = repo(pool, "g/b", Some(&sha('b'))).await;
    upload(pool, b, &sha('2'), "completed", 30).await;
    // Behind the tip but recent.
    let c = repo(pool, "g/c", Some(&sha('c'))).await;
    upload(pool, c, &sha('3'), "completed", 1).await;
    // An index job failed after the last completed upload.
    let d = repo(pool, "g/d", Some(&sha('d'))).await;
    upload(pool, d, &sha('4'), "completed", 2).await;
    sqlx::query(
        "INSERT INTO codeintel_index_jobs (repository_id, commit, indexer, state, failure_message,
                                           finished_at)
         VALUES ($1, $2, 'scip-go', 'failed', 'go: module not found', now())",
    )
    .bind(d)
    .bind(sha('d'))
    .execute(pool)
    .await
    .unwrap();
    // The latest upload failed, nothing completed.
    let e = repo(pool, "g/e", None).await;
    upload(pool, e, &sha('5'), "failed", 0).await;
    // Targeted by a policy, nothing indexed.
    let f = repo(pool, "g/f", Some(&sha('f'))).await;
    sqlx::query(
        "INSERT INTO codeintel_configuration_policies (repository_id, name, type, pattern,
                                                       indexing_enabled)
         VALUES ($1, 'f tip', 'GIT_TREE', 'HEAD', true)",
    )
    .bind(f)
    .execute(pool)
    .await
    .unwrap();
    // A failure before the newest completed upload does not count.
    let g = repo(pool, "g/g", Some(&sha('9'))).await;
    upload(pool, g, &sha('6'), "failed", 3).await;
    upload(pool, g, &sha('9'), "completed", 2).await;
    // Nothing at all.
    repo(pool, "g/h", Some(&sha('8'))).await;
    // Deleted repositories are left out.
    let gone = repo(pool, "g/gone", None).await;
    sqlx::query("UPDATE repo SET deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(pool)
        .await
        .unwrap();

    let run = |user, vars: serde_json::Value| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            let req = Request::new(COVERAGE).variables(Variables::from_json(vars));
            sourcegull_graphql::execute(&schema, req, actor).await
        }
    };
    let resp = run(alice, json!({"first": 10})).await;
    assert!(!resp.errors.is_empty(), "non-admins are refused");

    let resp = run(admin, json!({"first": 10})).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    let cov = &data["preciseIndexCoverage"];
    assert_eq!(cov["autoIndexingEnabled"], json!(true));
    assert_eq!(
        cov["counts"],
        json!({"upToDate": 3, "stale": 1, "failing": 2, "none": 1, "notTargeted": 1, "total": 8})
    );
    let states: Vec<(String, String)> = cov["repositories"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["repositoryName"].as_str().unwrap().to_owned(),
                n["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expect = [
        ("g/a", "UP_TO_DATE"),
        ("g/b", "STALE"),
        ("g/c", "UP_TO_DATE"),
        ("g/d", "FAILING"),
        ("g/e", "FAILING"),
        ("g/f", "NONE"),
        ("g/g", "UP_TO_DATE"),
        ("g/h", "NOT_TARGETED"),
    ];
    assert_eq!(
        states,
        expect
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect::<Vec<_>>()
    );
    let nodes = &cov["repositories"]["nodes"];
    assert_eq!(
        nodes[0],
        json!({"repositoryName": "g/a", "codeGraphURL": "/g/a/-/code-graph", "state": "UP_TO_DATE",
               "headCommit": sha('a'), "indexedCommit": sha('a'), "indexedAtHead": true,
               "failureMessage": null, "failureSource": null})
    );
    assert_eq!(nodes[3]["failureMessage"], json!("go: module not found"));
    assert_eq!(nodes[3]["failureSource"], json!("index_job"));
    assert_eq!(nodes[3]["indexedCommit"], json!(sha('4')));
    assert_eq!(nodes[4]["failureMessage"], json!("bad index"));
    assert_eq!(nodes[4]["failureSource"], json!("upload"));
    assert_eq!(nodes[4]["indexedCommit"], json!(null));

    // Filter by state, then page.
    let resp = run(admin, json!({"state": "FAILING", "first": 1})).await;
    assert!(resp.errors.is_empty(), "{:?}", resp.errors);
    let data = resp.data.into_json().unwrap();
    let page = &data["preciseIndexCoverage"]["repositories"];
    assert_eq!(page["totalCount"], json!(2));
    assert_eq!(page["nodes"][0]["repositoryName"], json!("g/d"));
    assert_eq!(page["pageInfo"]["hasNextPage"], json!(true));
    let after = page["pageInfo"]["endCursor"].clone();
    let resp = run(
        admin,
        json!({"state": "FAILING", "first": 1, "after": after}),
    )
    .await;
    let data = resp.data.into_json().unwrap();
    let page = &data["preciseIndexCoverage"]["repositories"];
    assert_eq!(page["nodes"][0]["repositoryName"], json!("g/e"));
    assert_eq!(page["pageInfo"]["hasNextPage"], json!(false));

    // Filter by name.
    let resp = run(admin, json!({"query": "G/B", "first": 10})).await;
    let data = resp.data.into_json().unwrap();
    let page = &data["preciseIndexCoverage"]["repositories"];
    assert_eq!(page["totalCount"], json!(1));
    assert_eq!(page["nodes"][0]["state"], json!("STALE"));

    // With auto-indexing off nothing is targeted.
    let targets = CoverageStore::new(&db).targets(false, false).await.unwrap();
    let counts = CoverageStore::new(&db)
        .counts(&sourcegull_codeintel_policies::store::CoverageOptions {
            targets,
            stale_after: chrono::TimeDelta::days(7),
        })
        .await
        .unwrap();
    assert_eq!((counts.none, counts.not_targeted), (0, 2));
    assert_eq!(counts.failing_index_jobs, 1);
}
