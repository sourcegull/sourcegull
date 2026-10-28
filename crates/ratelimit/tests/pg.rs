//! The shared rate limiter against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`). Each
//! `PgRateLimiters` stands for one process; they share buckets only through the database.

use std::sync::Arc;
use std::time::Duration;

use sourcegull_codehost::{RateLimiter, RateLimiters};
use sourcegull_db::migrate;
use sourcegull_ratelimit::PgRateLimiters;
use sourcegull_testutil::TestDb;
use tokio::time::Instant;

async fn setup() -> Option<TestDb> {
    let db = TestDb::new().await?;
    migrate::up(&db.pool).await.expect("migrate up");
    Some(db)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_share_one_budget() {
    let Some(db) = setup().await else { return };
    // 180,000/h = 50/s with a burst of 50; three "processes" with four tasks each.
    let rph = Some(180_000.0);
    let processes: Vec<_> = (0..3)
        .map(|_| PgRateLimiters::new(db.pool.clone()))
        .collect();
    let start = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for p in &processes {
        let limiter = p.get("extsvc:1", rph);
        for _ in 0..4 {
            let limiter = limiter.clone();
            tasks.spawn(async move {
                for _ in 0..15 {
                    limiter.acquire().await.unwrap();
                }
            });
        }
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    let elapsed = start.elapsed();
    // 180 requests: 50 from the burst, 130 more at 50/s = 2.6 s. Separate buckets per process
    // would have finished in 0.2 s.
    assert!(
        elapsed >= Duration::from_millis(2300),
        "finished too fast: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(6), "too slow: {elapsed:?}");
    let trips: u64 = processes
        .iter()
        .map(|p| p.get("extsvc:1", rph).round_trips())
        .sum();
    assert!(
        trips < 180,
        "tokens were not reserved in batches: {trips} round trips"
    );
    eprintln!("180 requests in {elapsed:?} with {trips} database round trips");

    let state = processes[0].state("extsvc:1", rph).await.unwrap();
    assert_eq!(
        (state.limit, state.burst, state.interval),
        (180_000, 50, 3600)
    );
    assert!(state.current_capacity < 50, "{state:?}");
    assert!(!state.infinite);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_requests_are_batched() {
    let Some(db) = setup().await else { return };
    let limiters = PgRateLimiters::new(db.pool.clone());
    let limiter = limiters.get("extsvc:2", Some(36_000_000.0));
    for _ in 0..500 {
        limiter.acquire().await.unwrap();
    }
    let trips = limiter.round_trips();
    assert!(trips < 100, "{trips} round trips for 500 requests");
    eprintln!("500 sequential requests with {trips} database round trips");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_stops_every_process() {
    let Some(db) = setup().await else { return };
    let a = PgRateLimiters::new(db.pool.clone());
    let b = PgRateLimiters::new(db.pool.clone());
    for (key, rph) in [("extsvc:3", Some(36_000.0)), ("extsvc:4", None)] {
        let start = Instant::now();
        a.get(key, rph)
            .pause_until(start + Duration::from_millis(1500))
            .await;
        let state = b.state(key, rph).await.unwrap();
        assert!(state.paused_until.is_some(), "{key}: {state:?}");
        b.get(key, rph).acquire().await.unwrap();
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(1300),
            "{key}: waited only {waited:?}"
        );
        assert!(b.state(key, rph).await.unwrap().paused_until.is_none());
    }
    assert!(b.state("extsvc:4", None).await.unwrap().infinite);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rates_states_and_fallback() {
    let Some(db) = setup().await else { return };
    let limiters = Arc::new(PgRateLimiters::new(db.pool.clone()));
    // Disabled.
    assert!(
        limiters
            .limiter("extsvc:5", Some(0.0))
            .acquire()
            .await
            .is_err()
    );
    // Untouched: a full bucket.
    let s = limiters.state("extsvc:6", Some(7200.0)).await.unwrap();
    assert_eq!((s.limit, s.burst, s.current_capacity), (7200, 2, 2));
    // Used, then the rate changes: the same limiter follows the new rate.
    let l = limiters.get("extsvc:6", Some(7200.0));
    l.acquire().await.unwrap();
    let s = limiters.state("extsvc:6", Some(7200.0)).await.unwrap();
    assert_eq!(s.current_capacity, 1);
    let l2 = limiters.get("extsvc:6", Some(36_000.0));
    assert!(Arc::ptr_eq(&l, &l2));
    l2.acquire().await.unwrap();
    let s = limiters.state("extsvc:6", Some(36_000.0)).await.unwrap();
    assert_eq!((s.limit, s.burst), (36_000, 10));

    // The database goes away: requests continue, limited in-process.
    db.pool.close().await;
    let l = limiters.get("extsvc:7", Some(36_000.0));
    let start = Instant::now();
    for _ in 0..12 {
        l.acquire().await.unwrap();
    }
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(limiters.state("extsvc:7", Some(36_000.0)).await.is_err());
}

#[tokio::test]
async fn keyed_counters_are_shared_between_processes() {
    use sourcegull_ratelimit::{KeyedLimiter, Limit, PgKeyedLimiter};
    let Some(db) = setup().await else { return };
    let a = PgKeyedLimiter::new(db.pool.clone());
    let b = PgKeyedLimiter::new(db.pool.clone());
    let limit = Limit::new(3, Duration::from_secs(60));
    assert!(a.check("sign-in:1.2.3.4", limit).await.is_ok());
    a.hit("sign-in:1.2.3.4", limit).await.unwrap();
    b.hit("sign-in:1.2.3.4", limit).await.unwrap();
    a.hit("sign-in:1.2.3.4", limit).await.unwrap();
    let err = b.hit("sign-in:1.2.3.4", limit).await.unwrap_err();
    assert!(
        err.retry_after > Duration::from_secs(55) && err.retry_after <= Duration::from_secs(60)
    );
    assert!(a.check("sign-in:1.2.3.4", limit).await.is_err());
    assert!(a.hit("sign-in:5.6.7.8", limit).await.is_ok());

    // A short window ends and starts over.
    let short = Limit::new(1, Duration::from_millis(300));
    a.hit("short", short).await.unwrap();
    assert!(b.hit("short", short).await.is_err());
    tokio::time::sleep(Duration::from_millis(400)).await;
    b.hit("short", short).await.unwrap();

    // Without the database the limit still applies in-process.
    db.pool.close().await;
    a.hit("offline", short).await.unwrap();
    assert!(a.hit("offline", short).await.is_err());
}
