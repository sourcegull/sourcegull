//! Background routine, outbound request, slow request, webhook log and feature flag stores
//! against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`).

use chrono::{Duration, Utc};
use serde_json::json;
use sourcegull_db::{
    Db, DbError, FeatureFlagValue, NewOutboundRequest, NewSlowRequest, NewUser, NewWebhookLog,
    OverrideNamespace, PgRoutineRecorder, RECENT_RUNS_KEPT, RoutineRecorder, RoutineRun,
    SlowRequestKind, WebhookLogFilter, migrate,
};
use sourcegull_testutil::TestDb;

async fn setup() -> Option<(TestDb, Db)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    Some((t, db))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routine_recorder_is_bounded() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let r = PgRoutineRecorder::new(db.pool().clone());
    r.started("janitor", "host-a").await.unwrap();
    r.started("janitor", "host-b").await.unwrap();
    r.stopped("janitor", "host-a").await.unwrap();
    for i in 0..(RECENT_RUNS_KEPT + 3) {
        let run = RoutineRun {
            at: Utc::now(),
            host_name: "host-b".into(),
            duration_ms: i64::try_from(i).unwrap() + 1,
            error: (i == 0).then(|| "boom".to_owned()),
        };
        r.record_run("janitor", &run).await.unwrap();
    }
    // Stats of days past the window are dropped.
    sqlx::query(
        "INSERT INTO background_routine_stats VALUES ('janitor', current_date - 30, 5, 5, 1, 1, 5)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    r.record_run(
        "other",
        &RoutineRun {
            at: Utc::now(),
            host_name: "host-b".into(),
            duration_ms: 7,
            error: None,
        },
    )
    .await
    .unwrap();

    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM background_routine_runs WHERE routine = 'janitor'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stored, i64::try_from(RECENT_RUNS_KEPT).unwrap());

    let snap = r.snapshot(3).await.unwrap();
    assert_eq!(
        snap.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["janitor", "other"]
    );
    let j = &snap[0];
    assert_eq!(j.recent_runs.len(), 3);
    assert_eq!(
        j.recent_runs[0].duration_ms,
        i64::try_from(RECENT_RUNS_KEPT).unwrap() + 3
    );
    assert_eq!(
        j.stats.run_count,
        i64::try_from(RECENT_RUNS_KEPT).unwrap() + 3
    );
    assert_eq!(j.stats.error_count, 1);
    assert_eq!(j.stats.min_duration_ms, 1);
    assert_eq!(
        j.stats.max_duration_ms,
        i64::try_from(RECENT_RUNS_KEPT).unwrap() + 3
    );
    assert_eq!(j.instances.len(), 2);
    let a = j
        .instances
        .iter()
        .find(|i| i.host_name == "host-a")
        .unwrap();
    let b = j
        .instances
        .iter()
        .find(|i| i.host_name == "host-b")
        .unwrap();
    assert!(!a.is_running());
    assert!(b.is_running());
    assert_eq!(snap[1].stats.run_count, 1);
}

fn request(n: i64) -> NewOutboundRequest {
    NewOutboundRequest {
        started_at: Utc::now(),
        method: "GET".into(),
        url: format!("https://gitlab.example.com/api/v4/projects?page={n}"),
        request_headers: json!({"private-token": ["REDACTED"]}),
        status_code: 200,
        duration_ms: n,
        creation_stack_frame: "gitlab https://gitlab.example.com/".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_request_ring() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.outbound_requests();
    let batch: Vec<_> = (1..=5).map(request).collect();
    store.insert_many(&batch, 3).await.unwrap();
    assert_eq!(store.count().await.unwrap(), 3);
    let page = store.list(None, 2).await.unwrap();
    assert_eq!(
        page.iter().map(|r| r.duration_ms).collect::<Vec<_>>(),
        [5, 4]
    );
    let rest = store.list(Some(page[1].id), 10).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].duration_ms, 3);
    assert_eq!(
        store
            .get(rest[0].id)
            .await
            .unwrap()
            .unwrap()
            .request_headers,
        json!({"private-token": ["REDACTED"]})
    );
    // A limit of 0 turns the log off and empties it.
    store.insert_many(&[request(6)], 0).await.unwrap();
    assert_eq!(store.count().await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_request_log() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.slow_requests();
    let new = |kind, name: &str| NewSlowRequest {
        started_at: Utc::now(),
        duration_ms: 1500.0,
        kind,
        user_id: None,
        name: name.into(),
        source: "browser".into(),
        repository: None,
        variables: "{}".into(),
        errors: vec!["oops".into()],
        query: "query X { a }".into(),
        filepath: None,
    };
    store
        .insert(&new(SlowRequestKind::GraphQl, "X"))
        .await
        .unwrap();
    store
        .insert(&new(SlowRequestKind::Search, "search"))
        .await
        .unwrap();
    assert_eq!(store.count(None).await.unwrap(), 2);
    assert_eq!(store.count(Some(SlowRequestKind::Search)).await.unwrap(), 1);
    let all = store.list(None, None, 10).await.unwrap();
    assert_eq!(all[0].name, "search");
    assert_eq!(all[1].errors, json!(["oops"]));
    let graphql = store
        .list(Some(SlowRequestKind::GraphQl), None, 10)
        .await
        .unwrap();
    assert_eq!(graphql.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_log_filters() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.webhook_logs();
    for status in [200, 401, 200] {
        store
            .insert(&NewWebhookLog {
                status_code: status,
                request: json!({"headers": {}, "body": "x"}),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let all = WebhookLogFilter::default();
    assert_eq!(store.count(&all).await.unwrap(), 3);
    let errors = WebhookLogFilter {
        only_errors: true,
        ..Default::default()
    };
    assert_eq!(store.count(&errors).await.unwrap(), 1);
    assert_eq!(store.search(&errors, 10).await.unwrap()[0].status_code, 401);
    let unmatched = WebhookLogFilter {
        only_unmatched: true,
        ..Default::default()
    };
    assert_eq!(store.count(&unmatched).await.unwrap(), 3);
    let page = store.search(&all, 2).await.unwrap();
    let next = store
        .search(
            &WebhookLogFilter {
                before_id: Some(page[1].id),
                ..Default::default()
            },
            2,
        )
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    let future = WebhookLogFilter {
        since: Some(Utc::now() + Duration::hours(1)),
        ..Default::default()
    };
    assert_eq!(store.count(&future).await.unwrap(), 0);
    assert!(store.get(page[0].id).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feature_flags_and_overrides() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.feature_flags();
    let user = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
        .get();
    let org = db.orgs().create("acme", None).await.unwrap().id.get();

    store
        .create("new-ui", FeatureFlagValue::Bool(false))
        .await
        .unwrap();
    assert!(matches!(
        store.create("new-ui", FeatureFlagValue::Bool(true)).await,
        Err(DbError::AlreadyExists { .. })
    ));
    assert!(matches!(
        store.create("bad", FeatureFlagValue::Rollout(20_000)).await,
        Err(DbError::Invalid(_))
    ));
    store
        .create("ranking", FeatureFlagValue::Rollout(2500))
        .await
        .unwrap();
    let updated = store
        .update("new-ui", FeatureFlagValue::Rollout(10_000))
        .await
        .unwrap();
    assert_eq!(updated.value, FeatureFlagValue::Rollout(10_000));
    assert!(matches!(
        store.update("missing", FeatureFlagValue::Bool(true)).await,
        Err(DbError::NotFound(_))
    ));
    assert_eq!(
        store
            .list()
            .await
            .unwrap()
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["new-ui", "ranking"]
    );

    store
        .create_override("ranking", OverrideNamespace::User(user), true)
        .await
        .unwrap();
    store
        .create_override("ranking", OverrideNamespace::Org(org), false)
        .await
        .unwrap();
    assert!(matches!(
        store
            .create_override("ranking", OverrideNamespace::User(user), false)
            .await,
        Err(DbError::AlreadyExists { .. })
    ));
    assert!(matches!(
        store
            .create_override("missing", OverrideNamespace::User(user), false)
            .await,
        Err(DbError::NotFound(_))
    ));
    let o = store
        .update_override("ranking", OverrideNamespace::User(user), false)
        .await
        .unwrap();
    assert!(!o.value);
    assert_eq!(store.overrides("ranking").await.unwrap().len(), 2);
    assert_eq!(
        store.overrides_for(Some(user), &[org]).await.unwrap().len(),
        2
    );
    assert_eq!(store.overrides_for(None, &[org]).await.unwrap().len(), 1);
    store
        .delete_override("ranking", OverrideNamespace::Org(org))
        .await
        .unwrap();
    assert!(
        store
            .get_override("ranking", OverrideNamespace::Org(org))
            .await
            .unwrap()
            .is_none()
    );

    // Deleting a flag deletes its overrides; the name can be used again.
    store.delete("ranking").await.unwrap();
    assert!(store.get("ranking").await.unwrap().is_none());
    assert!(store.overrides("ranking").await.unwrap().is_empty());
    assert!(matches!(
        store.delete("ranking").await,
        Err(DbError::NotFound(_))
    ));
    store
        .create("ranking", FeatureFlagValue::Bool(true))
        .await
        .unwrap();
}
