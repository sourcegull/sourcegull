//! Queue administration: discovery by shape, summaries, retry and cancel.

use sourcegull_pgqueue::admin;
use sourcegull_pgqueue::*;
use sourcegull_testutil::TestDb;

#[derive(Debug, sqlx::FromRow)]
struct Payload {
    n: i32,
}

#[tokio::test]
async fn summarize_retry_cancel() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let sql = create_queue_table_sql("admin_jobs", &["n integer NOT NULL"]).unwrap();
    sqlx::raw_sql(&sql).execute(&db.pool).await.unwrap();
    // Not a queue: lacks the standard columns.
    sqlx::raw_sql("CREATE TABLE not_a_queue (id bigserial PRIMARY KEY, state text)")
        .execute(&db.pool)
        .await
        .unwrap();
    let tables = admin::queue_tables(&db.pool).await.unwrap();
    assert!(tables.contains(&"admin_jobs".to_owned()), "{tables:?}");
    assert!(!tables.contains(&"not_a_queue".to_owned()));

    let q: Queue<Payload> = Queue::new(db.pool.clone(), QueueConfig::new("admin_jobs")).unwrap();
    sqlx::query("INSERT INTO admin_jobs (n) SELECT g FROM generate_series(1, 4) g")
        .execute(&db.pool)
        .await
        .unwrap();
    let jobs = q.dequeue("w", 2).await.unwrap();
    let (a, b) = (jobs[0].id, jobs[1].id);
    q.mark_failed(a, "w", "boom").await.unwrap();
    q.mark_complete(b, "w").await.unwrap();

    let s = admin::summarize(&db.pool, "admin_jobs", 5).await.unwrap();
    assert_eq!(
        (s.queued, s.processing, s.completed, s.failed),
        (2, 0, 1, 1)
    );
    assert!(s.oldest_queued_at.is_some());
    assert_eq!(s.recent_failures.len(), 1);
    assert_eq!(s.active.len(), 2);
    assert!(s.active.iter().all(|j| j.state == "queued"));
    assert_eq!(s.recent_failures[0].id, a);
    assert_eq!(
        s.recent_failures[0].failure_message.as_deref(),
        Some("boom")
    );

    // Only failed, errored or canceled jobs are retried.
    assert!(!admin::retry(&db.pool, "admin_jobs", b).await.unwrap());
    assert!(admin::retry(&db.pool, "admin_jobs", a).await.unwrap());
    let r = q.get(a).await.unwrap().unwrap();
    assert_eq!(r.state, JobState::Queued);
    assert_eq!(r.num_failures, 0);
    assert!(r.payload.n >= 1);
    assert!(r.failure_message.is_none());

    // Cancel: a queued job is canceled at once; a finished one cannot be.
    assert!(admin::cancel(&db.pool, "admin_jobs", a).await.unwrap());
    assert_eq!(q.get(a).await.unwrap().unwrap().state, JobState::Canceled);
    assert!(!admin::cancel(&db.pool, "admin_jobs", b).await.unwrap());

    // Table names are validated before they reach SQL.
    assert!(
        admin::summarize(&db.pool, "x; DROP TABLE admin_jobs", 1)
            .await
            .is_err()
    );
}
