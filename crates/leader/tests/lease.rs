use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use sourcegull_leader::*;
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;

async fn wait_for(timeout: Duration, f: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_one_holder() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let a = PgAdvisoryLease::from_pool(&db.pool, "janitor");
    let b = PgAdvisoryLease::from_pool(&db.pool, "janitor");
    let other = PgAdvisoryLease::from_pool(&db.pool, "scheduler");
    assert!(a.try_acquire().await.unwrap());
    assert!(
        a.try_acquire().await.unwrap(),
        "re-acquire while held is a renew"
    );
    assert!(!b.try_acquire().await.unwrap());
    assert!(!b.renew().await.unwrap());
    assert!(other.try_acquire().await.unwrap());
    assert!(a.renew().await.unwrap());
    a.release().await;
    assert!(!a.renew().await.unwrap());
    assert!(b.try_acquire().await.unwrap());
    b.release().await;
    other.release().await;
}

struct Contender {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

fn contender(db: &TestDb, name: &str) -> Contender {
    let running = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let backend = PgAdvisoryLease::from_pool(&db.pool, name).with_timeout(Duration::from_secs(2));
    let (r, s, t) = (running.clone(), starts.clone(), stops.clone());
    let name = name.to_owned();
    let c = cancel.clone();
    let handle = tokio::spawn(async move {
        run_singleton(
            &name,
            backend,
            None,
            Duration::from_millis(200),
            move |token, _fence| {
                let (r, s, t) = (r.clone(), s.clone(), t.clone());
                async move {
                    r.store(true, Ordering::SeqCst);
                    s.fetch_add(1, Ordering::SeqCst);
                    token.cancelled().await;
                    r.store(false, Ordering::SeqCst);
                    t.fetch_add(1, Ordering::SeqCst);
                }
            },
            c,
        )
        .await;
    });
    Contender {
        running,
        starts,
        stops,
        cancel,
        handle,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn takeover_after_leader_connection_terminated() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let a = contender(&db, "repo-discovery");
    let b = contender(&db, "repo-discovery");
    let leading = |c: &Contender| c.running.load(Ordering::SeqCst);

    assert!(wait_for(Duration::from_secs(3), || leading(&a) || leading(&b)).await);
    // Several renew intervals pass with exactly one leader.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(leading(&a) ^ leading(&b));
    let (leader, follower) = if leading(&a) { (&a, &b) } else { (&b, &a) };
    assert_eq!(follower.starts.load(Ordering::SeqCst), 0);

    let key = advisory_key("repo-discovery");
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks
         WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND classid::bigint = ($1 >> 32) & 4294967295
           AND objid::bigint = $1 & 4294967295",
    )
    .bind(key)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(killed, vec![true]);

    assert!(
        wait_for(Duration::from_secs(3), || leader
            .stops
            .load(Ordering::SeqCst)
            == 1)
        .await
    );
    assert!(wait_for(Duration::from_secs(3), || leading(follower)).await);
    assert_eq!(follower.starts.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !leading(leader),
        "old leader must not regain the lease while the new one holds it"
    );

    for c in [&a, &b] {
        c.cancel.cancel();
    }
    for c in [a, b] {
        tokio::time::timeout(Duration::from_secs(3), c.handle)
            .await
            .unwrap()
            .unwrap();
        assert!(!c.running.load(Ordering::SeqCst));
    }
}

/// Advisory locks held by `pid`.
async fn locks_of(db: &TestDb, pid: i32) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted AND pid = $1",
    )
    .bind(pid)
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

async fn leader_sessions(db: &TestDb) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE application_name = 'sourcegull-leader' AND datname = current_database()",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_leases_share_one_connection() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let m = PgLeaseManager::from_pool(&db.pool);
    let leases: Vec<_> = (0..16).map(|i| m.lease(format!("job-{i}"))).collect();
    for l in &leases {
        assert!(l.try_acquire().await.unwrap(), "{}", l.name());
    }
    assert_eq!(leader_sessions(&db).await, 1);
    let pid = m.backend_pid().await.unwrap();
    assert_eq!(locks_of(&db, pid).await, 16);

    // A second lease of the same name on the same manager doesn't get it
    // (advisory locks are reentrant within a session).
    let dup = m.lease("job-3");
    assert!(!dup.try_acquire().await.unwrap());
    assert!(!dup.renew().await.unwrap());
    dup.release().await;
    assert!(leases[3].renew().await.unwrap());

    // Releasing one lease leaves the others held.
    leases[0].release().await;
    assert!(!leases[0].renew().await.unwrap());
    assert_eq!(locks_of(&db, pid).await, 15);
    for l in &leases[1..] {
        assert!(l.renew().await.unwrap(), "{}", l.name());
    }
    // ... and it can be taken again on the same session.
    assert!(leases[0].try_acquire().await.unwrap());
    assert_eq!(m.backend_pid().await, Some(pid));
    for l in &leases {
        l.release().await;
    }
    assert_eq!(locks_of(&db, pid).await, 0);
    assert_eq!(
        leader_sessions(&db).await,
        1,
        "session stays open for reuse"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contention_across_managers() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let m1 = PgLeaseManager::from_pool(&db.pool);
    let m2 = PgLeaseManager::from_pool(&db.pool);
    let (a1, b1) = (m1.lease("a"), m1.lease("b"));
    let (a2, b2, c2) = (m2.lease("a"), m2.lease("b"), m2.lease("c"));
    assert!(a1.try_acquire().await.unwrap());
    assert!(b2.try_acquire().await.unwrap());
    assert!(c2.try_acquire().await.unwrap());
    assert!(!a2.try_acquire().await.unwrap());
    assert!(!b1.try_acquire().await.unwrap());
    assert_eq!(leader_sessions(&db).await, 2);

    a1.release().await;
    assert!(a2.try_acquire().await.unwrap());
    assert!(b2.renew().await.unwrap() && c2.renew().await.unwrap());
    b2.release().await;
    assert!(b1.try_acquire().await.unwrap());
    assert!(!a1.try_acquire().await.unwrap());
    for l in [&a1, &b1, &a2, &b2, &c2] {
        l.release().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_loss_loses_every_lease_then_reacquires() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let m = PgLeaseManager::from_pool(&db.pool).with_timeout(Duration::from_secs(2));
    let leases: Vec<_> = (0..4).map(|i| Arc::new(m.lease(format!("l{i}")))).collect();
    for l in &leases {
        assert!(l.try_acquire().await.unwrap());
    }
    let watchers: Vec<_> = leases
        .iter()
        .map(|l| {
            let l = l.clone();
            tokio::spawn(async move { l.lost().await })
        })
        .collect();
    let pid = m.backend_pid().await.unwrap();
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(killed);
    tokio::time::sleep(Duration::from_millis(200)).await;
    for w in &watchers {
        assert!(!w.is_finished(), "nothing noticed the loss yet");
    }

    // The first renewal to hit the dead connection loses every lease at once.
    assert!(leases[0].renew().await.is_err());
    for w in watchers {
        tokio::time::timeout(Duration::from_secs(1), w)
            .await
            .expect("lost() resolves for every lease")
            .unwrap();
    }
    for l in &leases[1..] {
        assert!(!l.renew().await.unwrap());
    }

    // Reconnects and re-acquires on one new session.
    for l in &leases {
        assert!(l.try_acquire().await.unwrap());
    }
    let new_pid = m.backend_pid().await.unwrap();
    assert_ne!(new_pid, pid);
    assert_eq!(locks_of(&db, new_pid).await, 4);
    for l in &leases {
        l.release().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn singletons_on_one_manager_stop_together_on_connection_loss() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let m = PgLeaseManager::from_pool(&db.pool).with_timeout(Duration::from_secs(2));
    let cancel = CancellationToken::new();
    let starts: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let stops: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let mut handles = Vec::new();
    for i in 0..3 {
        let name = format!("s{i}");
        let backend = m.lease(&name);
        let (s, t) = (starts[i].clone(), stops[i].clone());
        let c = cancel.clone();
        // Long renew interval for all but the first, so only lost() can stop them quickly.
        let interval = Duration::from_millis(if i == 0 { 200 } else { 60_000 });
        handles.push(tokio::spawn(async move {
            run_singleton(
                &name,
                backend,
                None,
                interval,
                move |token, _fence| {
                    let (s, t) = (s.clone(), t.clone());
                    async move {
                        s.fetch_add(1, Ordering::SeqCst);
                        token.cancelled().await;
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                },
                c,
            )
            .await;
        }));
    }
    let all = |v: &[Arc<AtomicUsize>], n| v.iter().all(|x| x.load(Ordering::SeqCst) == n);
    assert!(wait_for(Duration::from_secs(3), || all(&starts, 1)).await);
    assert_eq!(leader_sessions(&db).await, 1);

    let pid = m.backend_pid().await.unwrap();
    let _: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        wait_for(Duration::from_secs(3), || all(&stops, 1)).await,
        "every singleton on the lost connection stops"
    );
    // s0 re-acquires on a new session after its next tick.
    assert!(
        wait_for(Duration::from_secs(3), || starts[0].load(Ordering::SeqCst)
            == 2)
        .await
    );

    cancel.cancel();
    for h in handles {
        tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .unwrap()
            .unwrap();
    }
}
