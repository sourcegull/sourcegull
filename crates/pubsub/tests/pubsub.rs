//! The shared listener against a real Postgres.

use std::time::Duration;

use sourcegull_pubsub::{Event, subscribe};
use sourcegull_testutil::TestDb;
use sqlx::PgPool;

async fn listen_connections(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND application_name LIKE '%:listen'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn notify(pool: &PgPool, channel: &str, payload: &str) {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(channel)
        .bind(payload)
        .execute(pool)
        .await
        .unwrap();
}

async fn next(sub: &mut sourcegull_pubsub::Subscription) -> Event {
    tokio::time::timeout(Duration::from_secs(10), sub.recv())
        .await
        .expect("an event within 10 s")
}

fn note(channel: &str, payload: &str) -> Event {
    Event::Notification {
        channel: channel.to_owned(),
        payload: payload.to_owned(),
    }
}

#[tokio::test]
async fn subscribers_share_one_connection_and_resync_after_reconnect() {
    let Some(t) = TestDb::new().await else { return };
    let mut a = subscribe(&t.pool, &["chan_a"]).await;
    let mut ab = subscribe(&t.pool, &["chan_a", "chan_b"]).await;
    let mut b = subscribe(&t.pool, &["chan_b"]).await;
    assert!(a.is_connected());
    assert_eq!(next(&mut a).await, Event::Resync);
    assert_eq!(next(&mut ab).await, Event::Resync);
    assert_eq!(next(&mut b).await, Event::Resync);
    assert_eq!(listen_connections(&t.pool).await, 1);

    notify(&t.pool, "chan_b", "1").await;
    notify(&t.pool, "chan_a", "2").await;
    assert_eq!(next(&mut a).await, note("chan_a", "2"));
    assert_eq!(next(&mut ab).await, note("chan_b", "1"));
    assert_eq!(next(&mut ab).await, note("chan_a", "2"));
    assert_eq!(next(&mut b).await, note("chan_b", "1"));

    // Kill the shared connection: everyone resyncs and notifications flow again.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
         WHERE datname = current_database() AND application_name LIKE '%:listen'",
    )
    .execute(&t.pool)
    .await
    .unwrap();
    assert_eq!(next(&mut a).await, Event::Resync);
    assert_eq!(next(&mut b).await, Event::Resync);
    // Wait until the replacement connection listens again.
    for _ in 0..100 {
        if listen_connections(&t.pool).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    notify(&t.pool, "chan_a", "3").await;
    assert_eq!(next(&mut a).await, note("chan_a", "3"));

    // The connection closes with the last subscription.
    drop((a, ab, b));
    for _ in 0..100 {
        if listen_connections(&t.pool).await == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the listener connection outlived its subscriptions");
}
