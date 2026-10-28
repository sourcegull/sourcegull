//! Background jobs, outbound requests, slow requests, webhook logs and feature flags in the
//! admin API against the test Postgres: site admins only, and the shapes
//! of `backgroundJobs`, `outboundRequests`, `slowRequests`, `webhookLogs` and
//! feature flag operations.

use std::sync::Arc;
use std::time::Duration;

use async_graphql::{Request, Variables};
use axum::body::Body;
use axum::http::{self, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull::features::graphql_schema as build_schema;
use sourcegull_authn::{AuthVia, Authenticator, MemorySessionStore, PasswordParams};
use sourcegull_authz::Checker;
use sourcegull_config::{ConfigHandle, SiteConfig};
use sourcegull_db::{
    Db, NewOutboundRequest, NewUser, NewWebhookLog, PgRoutineRecorder, RoutineRun, migrate,
};
use sourcegull_graphql::Services;
use sourcegull_services::relay::marshal_id;
use sourcegull_services::{AppState, SiteConfigSource};
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const JOBS: &str = "{ backgroundJobs(recentRunCount: 2) { totalCount nodes { id name routines {
    name type description intervalMs
    instances { hostName lastStartedAt lastStoppedAt }
    recentRuns { at hostName durationMs errorMessage }
    stats { since runCount errorCount minDurationMs avgDurationMs maxDurationMs }
    queue { table queued processing completed errored failed canceled oldestQueuedAt
            recentFailures { id state failureMessage numFailures }
            activeJobs { id state queuedAt startedAt } }
} } } }";

const OUTBOUND: &str = "{ outboundRequests(first: 10) { totalCount nodes {
    id startedAt method url requestHeaders { name values } requestBody statusCode
    responseHeaders { name values } durationMs errorMessage creationStackFrame callStack
} pageInfo { hasNextPage } } }";

const SLOW: &str = "{ slowRequests { totalCount nodes {
    index start duration user { username } name source repository { name } variables errors
    query filepath kind
} } }";

const WEBHOOKS: &str = "{ webhookLogs(onlyErrors: true) { totalCount nodes {
    id receivedAt externalService { id } statusCode event
    request { headers { name values } body method url version }
    response { headers { name values } body }
} } }";

const FLAGS: &str = "{ featureFlags {
    __typename
    ... on FeatureFlagBoolean { name value overrides { id value namespace { id } targetFlag { __typename } } }
    ... on FeatureFlagRollout { name rolloutBasisPoints overrides { id value } }
} }";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_logs_api() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    // Every request is "slow".
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            observability_log_slow_graphql_requests: Some(0),
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
        authz: Checker::new(db.clone(), config.clone()),
        app: None,
    });
    let run = |user, query: String, vars: Value| {
        let (schema, auth) = (schema.clone(), auth.clone());
        async move {
            let actor = auth.actor_for(user, AuthVia::Session).await.unwrap();
            let resp = sourcegull_graphql::execute(
                &schema,
                Request::new(query).variables(Variables::from_json(vars)),
                actor,
            )
            .await;
            (
                resp.errors
                    .iter()
                    .map(|e| e.message.clone())
                    .collect::<Vec<_>>(),
                resp.data.into_json().unwrap(),
            )
        }
    };
    let ok = |(errors, data): (Vec<String>, Value)| {
        assert!(errors.is_empty(), "{errors:?}");
        data
    };

    // Site admins only.
    for q in [JOBS, OUTBOUND, SLOW, WEBHOOKS, FLAGS] {
        let (errors, _) = run(alice, q.to_owned(), json!({})).await;
        assert!(!errors.is_empty(), "non-admin ran {q}");
    }
    for m in [
        "mutation { createFeatureFlag(name: \"x\", value: true) { __typename } }",
        "mutation { retryBackgroundQueueJob(queue: \"admin_test_jobs\", id: \"1\") { alwaysNil } }",
        "mutation { cancelBackgroundQueueJob(queue: \"admin_test_jobs\", id: \"1\") { alwaysNil } }",
    ] {
        let (errors, _) = run(alice, m.to_owned(), json!({})).await;
        assert!(!errors.is_empty(), "non-admin ran {m}");
    }

    // Background jobs: a recorded routine and a queue with a failed job.
    sourcegull_db::set_global_recorder(Arc::new(PgRoutineRecorder::new(db.pool().clone())));
    let rec = sourcegull_db::global_recorder();
    rec.started("repo-sync-scheduler", "worker-1")
        .await
        .unwrap();
    for (ms, err) in [(10, None), (30, Some("db down"))] {
        rec.record_run(
            "repo-sync-scheduler",
            &RoutineRun {
                at: chrono::Utc::now(),
                host_name: "worker-1".into(),
                duration_ms: ms,
                error: err.map(str::to_owned),
            },
        )
        .await
        .unwrap();
    }
    let result: Result<(), std::io::Error> = sourcegull_db::record_run("custom-routine", async {
        Err(std::io::Error::other("nope"))
    })
    .await;
    assert!(result.is_err());
    sqlx::raw_sql(
        &sourcegull_pgqueue::create_queue_table_sql("admin_test_jobs", &["n integer"]).unwrap(),
    )
    .execute(db.pool())
    .await
    .unwrap();
    let failed: i64 = sqlx::query_scalar(
        "INSERT INTO admin_test_jobs (state, failure_message, finished_at, num_failures)
         VALUES ('failed', 'exploded', now(), 3) RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO admin_test_jobs (n) VALUES (1)")
        .execute(db.pool())
        .await
        .unwrap();

    let data = ok(run(admin, JOBS.to_owned(), json!({})).await);
    let jobs = data["backgroundJobs"]["nodes"].as_array().unwrap();
    assert_eq!(data["backgroundJobs"]["totalCount"], 2);
    let routines = jobs[0]["routines"].as_array().unwrap();
    let sync = routines
        .iter()
        .find(|r| r["name"] == "repo-sync-scheduler")
        .unwrap();
    assert_eq!(sync["type"], "PERIODIC");
    assert_eq!(sync["intervalMs"], 10_000);
    assert!(!sync["description"].as_str().unwrap().is_empty());
    assert_eq!(sync["instances"][0]["hostName"], "worker-1");
    assert!(sync["instances"][0]["lastStoppedAt"].is_null());
    assert_eq!(sync["recentRuns"].as_array().unwrap().len(), 2);
    assert_eq!(sync["recentRuns"][0]["errorMessage"], "db down");
    assert_eq!(sync["stats"]["runCount"], 2);
    assert_eq!(sync["stats"]["errorCount"], 1);
    assert_eq!(sync["stats"]["avgDurationMs"], 20);
    // Known routines that never ran are listed too; unknown recorded ones as well.
    assert!(routines.iter().any(|r| r["name"] == "insights-janitor"));
    let custom = routines
        .iter()
        .find(|r| r["name"] == "custom-routine")
        .unwrap();
    assert_eq!(custom["recentRuns"][0]["errorMessage"], "nope");

    let queues = jobs[1]["routines"].as_array().unwrap();
    assert!(queues.iter().all(|q| q["type"] == "DB_BACKED"));
    assert!(
        queues
            .iter()
            .any(|q| q["name"] == "external_service_sync_jobs")
    );
    let q = &queues
        .iter()
        .find(|q| q["name"] == "admin_test_jobs")
        .unwrap()["queue"];
    assert_eq!(
        (q["queued"].clone(), q["failed"].clone()),
        (json!(1), json!(1))
    );
    assert!(!q["oldestQueuedAt"].is_null());
    assert_eq!(q["recentFailures"][0]["id"], failed.to_string());
    assert_eq!(q["recentFailures"][0]["failureMessage"], "exploded");
    assert_eq!(q["activeJobs"][0]["state"], "queued");

    let job_id = jobs[0]["id"].as_str().unwrap().to_owned();
    let data = ok(run(
        admin,
        "query($id: ID!) { node(id: $id) { ... on BackgroundJob { name } } }".to_owned(),
        json!({ "id": job_id }),
    )
    .await);
    assert_eq!(data["node"]["name"], jobs[0]["name"]);

    let retry = "mutation($q: String!, $id: ID!) { retryBackgroundQueueJob(queue: $q, id: $id) { alwaysNil } }";
    let cancel = "mutation($q: String!, $id: ID!) { cancelBackgroundQueueJob(queue: $q, id: $id) { alwaysNil } }";
    ok(run(
        admin,
        retry.to_owned(),
        json!({"q": "admin_test_jobs", "id": failed.to_string()}),
    )
    .await);
    let state: String = sqlx::query_scalar("SELECT state FROM admin_test_jobs WHERE id = $1")
        .bind(failed)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(state, "queued");
    // Retrying a queued job, or touching a table that is not a queue, fails.
    let (errors, _) = run(
        admin,
        retry.to_owned(),
        json!({"q": "admin_test_jobs", "id": failed.to_string()}),
    )
    .await;
    assert!(!errors.is_empty());
    let (errors, _) = run(admin, retry.to_owned(), json!({"q": "users", "id": "1"})).await;
    assert!(!errors.is_empty());
    ok(run(
        admin,
        cancel.to_owned(),
        json!({"q": "admin_test_jobs", "id": failed.to_string()}),
    )
    .await);
    let state: String = sqlx::query_scalar("SELECT state FROM admin_test_jobs WHERE id = $1")
        .bind(failed)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(state, "canceled");

    // Outbound requests.
    db.outbound_requests()
        .insert_many(
            &[NewOutboundRequest {
                started_at: chrono::Utc::now(),
                method: "GET".into(),
                url: "https://gitlab.example.com/api/v4/version?private_token=REDACTED".into(),
                request_headers: json!({"private-token": ["REDACTED"]}),
                status_code: 200,
                response_headers: json!({"content-type": ["application/json"]}),
                duration_ms: 12,
                creation_stack_frame: "gitlab https://gitlab.example.com/".into(),
                ..Default::default()
            }],
            10,
        )
        .await
        .unwrap();
    let data = ok(run(admin, OUTBOUND.to_owned(), json!({})).await);
    let r = &data["outboundRequests"];
    assert_eq!(r["totalCount"], 1);
    assert_eq!(r["nodes"][0]["statusCode"], 200);
    assert_eq!(
        r["nodes"][0]["requestHeaders"],
        json!([{"name": "private-token", "values": ["REDACTED"]}])
    );
    assert_eq!(r["nodes"][0]["durationMs"], 12);
    let id = r["nodes"][0]["id"].as_str().unwrap().to_owned();
    let data = ok(run(
        admin,
        "query($id: ID!) { node(id: $id) { ... on OutboundRequest { method } } }".to_owned(),
        json!({ "id": id }),
    )
    .await);
    assert_eq!(data["node"]["method"], "GET");

    // Webhook logs, global and per connection.
    db.webhook_logs()
        .insert(&NewWebhookLog {
            status_code: 401,
            event: Some("Push Hook".into()),
            request: json!({"method": "POST", "url": "/.api/gitlab-webhooks",
                            "headers": {"x-gitlab-event": "Push Hook"}, "body": {"ref": "main"}}),
            error: Some("invalid secret".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    db.webhook_logs()
        .insert(&NewWebhookLog {
            status_code: 200,
            request: json!({"headers": {}, "body": ""}),
            ..Default::default()
        })
        .await
        .unwrap();
    let data = ok(run(admin, WEBHOOKS.to_owned(), json!({})).await);
    let w = &data["webhookLogs"];
    assert_eq!(w["totalCount"], 1);
    let n = &w["nodes"][0];
    assert_eq!(n["statusCode"], 401);
    assert_eq!(n["event"], "Push Hook");
    assert!(n["externalService"].is_null());
    assert_eq!(n["request"]["url"], "/.api/gitlab-webhooks");
    assert_eq!(
        n["request"]["headers"],
        json!([{"name": "x-gitlab-event", "values": ["Push Hook"]}])
    );
    assert!(n["request"]["body"].as_str().unwrap().contains("\"ref\""));
    assert_eq!(n["response"]["body"], "invalid secret");

    // Feature flags and overrides.
    let create = "mutation($name: String!, $value: Boolean, $rollout: Int) {
        createFeatureFlag(name: $name, value: $value, rolloutBasisPoints: $rollout) {
            __typename ... on FeatureFlagBoolean { name value } ... on FeatureFlagRollout { name rolloutBasisPoints } } }";
    let data = ok(run(
        admin,
        create.to_owned(),
        json!({"name": "new-ui", "value": false}),
    )
    .await);
    assert_eq!(
        data["createFeatureFlag"]["__typename"],
        "FeatureFlagBoolean"
    );
    let data = ok(run(
        admin,
        create.to_owned(),
        json!({"name": "ranking", "rollout": 0}),
    )
    .await);
    assert_eq!(data["createFeatureFlag"]["rolloutBasisPoints"], 0);
    let (errors, _) = run(
        admin,
        create.to_owned(),
        json!({"name": "new-ui", "value": true}),
    )
    .await;
    assert!(!errors.is_empty(), "duplicate name");
    let (errors, _) = run(
        admin,
        create.to_owned(),
        json!({"name": "both", "value": true, "rollout": 5}),
    )
    .await;
    assert!(!errors.is_empty(), "value and rollout");

    let alice_id = marshal_id("User", alice.get());
    let data = ok(run(
        admin,
        "mutation($ns: ID!) { createFeatureFlagOverride(namespace: $ns, flagName: \"new-ui\", value: true) { id value } }".to_owned(),
        json!({"ns": alice_id}),
    )
    .await);
    let override_id = data["createFeatureFlagOverride"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let data = ok(run(admin, FLAGS.to_owned(), json!({})).await);
    let flags = data["featureFlags"].as_array().unwrap();
    assert_eq!(flags.len(), 2);
    assert_eq!(flags[0]["name"], "new-ui");
    assert_eq!(flags[0]["overrides"][0]["namespace"]["id"], alice_id);
    assert_eq!(
        flags[0]["overrides"][0]["targetFlag"]["__typename"],
        "FeatureFlagBoolean"
    );

    // Evaluation: alice's override wins; the admin gets the default.
    let eval = "{ evaluateFeatureFlag(flagName: \"new-ui\") missing: evaluateFeatureFlag(flagName: \"nope\") evaluatedFeatureFlags { name value } }";
    let data = ok(run(alice, eval.to_owned(), json!({})).await);
    assert_eq!(data["evaluateFeatureFlag"], true);
    assert!(data["missing"].is_null());
    assert_eq!(data["evaluatedFeatureFlags"].as_array().unwrap().len(), 2);
    let data = ok(run(admin, eval.to_owned(), json!({})).await);
    assert_eq!(data["evaluateFeatureFlag"], false);

    ok(run(
        admin,
        "mutation($id: ID!) { updateFeatureFlagOverride(id: $id, value: false) { value } }"
            .to_owned(),
        json!({"id": override_id}),
    )
    .await);
    let data = ok(run(alice, eval.to_owned(), json!({})).await);
    assert_eq!(data["evaluateFeatureFlag"], false);
    let data = ok(run(
        admin,
        "mutation { updateFeatureFlag(name: \"ranking\", rolloutBasisPoints: 10000) { ... on FeatureFlagRollout { rolloutBasisPoints } } }".to_owned(),
        json!({}),
    )
    .await);
    assert_eq!(data["updateFeatureFlag"]["rolloutBasisPoints"], 10_000);
    let data = ok(run(
        alice,
        "{ evaluateFeatureFlag(flagName: \"ranking\") }".to_owned(),
        json!({}),
    )
    .await);
    assert_eq!(data["evaluateFeatureFlag"], true);
    ok(run(
        admin,
        "mutation($id: ID!) { deleteFeatureFlagOverride(id: $id) { alwaysNil } }".to_owned(),
        json!({"id": override_id}),
    )
    .await);
    ok(run(
        admin,
        "mutation { deleteFeatureFlag(name: \"ranking\") { alwaysNil } }".to_owned(),
        json!({}),
    )
    .await);
    let data = ok(run(admin, FLAGS.to_owned(), json!({})).await);
    assert_eq!(data["featureFlags"].as_array().unwrap().len(), 1);
    assert!(
        data["featureFlags"][0]["overrides"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Slow GraphQL requests over HTTP (the threshold is 0 ms).
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = sourcegull_observability::MetricsHandle::from_prometheus(recorder.handle());
    let state = AppState::new(db.clone(), config.clone(), metrics, auth.clone());
    let router = sourcegull_http_api::finish(
        sourcegull_http_api::public_routes().merge(sourcegull::features::graphql_routes(&state)),
        state,
    );
    let (_, secret) = auth
        .create_access_token(
            &auth
                .actor_for(admin, AuthVia::Session)
                .await
                .unwrap()
                .unwrap(),
            admin,
            &["user:all".into()],
            "t",
            None,
        )
        .await
        .unwrap();
    let body = json!({"query": "query Me { currentUser { username } }", "operationName": "Me"});
    let resp = router
        .clone()
        .oneshot(
            http::Request::post("/.api/graphql?Me")
                .header("authorization", format!("token {secret}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.into_body().collect().await.unwrap();
    let mut slow = Value::Null;
    for _ in 0..100 {
        slow = ok(run(admin, SLOW.to_owned(), json!({})).await);
        if slow["slowRequests"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Me")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let me = slow["slowRequests"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Me")
        .expect("slow request logged")
        .clone();
    assert_eq!(me["user"]["username"], "admin");
    assert_eq!(me["kind"], "GRAPHQL");
    assert_eq!(me["source"], "other");
    assert!(me["query"].as_str().unwrap().contains("currentUser"));
    assert!(me["duration"].as_f64().unwrap() >= 0.0);
}
