//! An evaluator that lost its lease queues no trigger jobs and does not move the cursor,
//! against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`).

use chrono::Utc;
use sourcegull_db::{Db, Fence, Namespace, NewUser, migrate, sqlx};
use sourcegull_monitors::EVALUATOR;
use sourcegull_monitors::store::{MonitorStore, NewMonitor};
use sourcegull_testutil::TestDb;

#[tokio::test]
async fn stale_evaluator_writes_are_refused() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let store = MonitorStore::new(&db);
    let monitor = store
        .create(&NewMonitor {
            namespace: Namespace::User(alice),
            description: "TODOs".into(),
            enabled: true,
            query: "type:diff TODO".into(),
            actions: Vec::new(),
            created_by: alice,
            unfiltered: false,
        })
        .await
        .unwrap();
    let jobs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM cm_trigger_jobs")
            .fetch_one(&t.pool)
            .await
            .unwrap()
    };
    let job = vec![(monitor.id, "type:diff TODO".to_owned(), Vec::new())];

    let stale = Fence::advance(&t.pool, EVALUATOR).await.unwrap();
    store
        .record_evaluation(&[], &[], &job, &[], Utc::now(), &stale)
        .await
        .unwrap();
    store.set_cursor(5, &stale).await.unwrap();

    // A successor takes the lease: the old evaluator's next pass writes nothing.
    let current = Fence::advance(&t.pool, EVALUATOR).await.unwrap();
    let err = store
        .record_evaluation(&[], &[], &job, &[], Utc::now(), &stale)
        .await
        .unwrap_err();
    assert!(err.is_fenced(), "{err}");
    assert!(store.set_cursor(9, &stale).await.unwrap_err().is_fenced());
    assert_eq!(jobs().await, 1);
    assert_eq!(store.cursor().await.unwrap(), 5);

    store
        .record_evaluation(&[], &[], &job, &[], Utc::now(), &current)
        .await
        .unwrap();
    store.set_cursor(9, &current).await.unwrap();
    assert_eq!(jobs().await, 2);
    assert_eq!(store.cursor().await.unwrap(), 9);
}
