//! Lease epochs: a leader that lost its lease cannot write over its successor.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sourcegull_db::{Db, DbError, MigrationDescriptor, MinorVersion, migrate};
use sourcegull_leader::*;
use sourcegull_testutil::TestDb;
use tokio_util::sync::CancellationToken;

async fn migrated() -> Option<TestDb> {
    let db = TestDb::new().await?;
    migrate::up(&db.pool).await.unwrap();
    Some(db)
}

async fn register_migration(db: &Db) {
    let d = MigrationDescriptor {
        id: 1,
        team: "source",
        component: "encryption",
        description: "re-encrypt",
        introduced: MinorVersion::new(0, 1),
        deprecated: None,
        non_destructive: true,
    };
    db.oob_migrations().register(&d, 0.0).await.unwrap();
}

async fn progress(db: &Db) -> f64 {
    db.oob_migrations().get(1).await.unwrap().unwrap().progress
}

#[tokio::test]
async fn stale_fence_is_refused_after_a_successor_acquires() {
    let Some(t) = migrated().await else { return };
    let db = Db::from_pool(t.pool.clone());
    register_migration(&db).await;
    let store = db.oob_migrations();

    let a = PgAdvisoryLease::from_pool(&t.pool, "runner");
    let b = PgAdvisoryLease::from_pool(&t.pool, "runner");
    assert!(a.try_acquire().await.unwrap());
    let stale = Fence::advance(&t.pool, "runner").await.unwrap();
    store.update_progress(1, 0.25, &stale).await.unwrap();

    // A loses the lease (e.g. paused past its renewals) and B takes it over.
    a.release().await;
    assert!(b.try_acquire().await.unwrap());
    let current = Fence::advance(&t.pool, "runner").await.unwrap();
    assert_eq!(current.epoch(), stale.epoch().map(|e| e + 1));

    let err = store.update_progress(1, 0.1, &stale).await.unwrap_err();
    assert!(err.is_fenced(), "{err}");
    assert!(matches!(
        err,
        DbError::Fenced {
            epoch: 1,
            current: Some(2),
            ..
        }
    ));
    assert!((progress(&db).await - 0.25).abs() < 1e-9);

    store.update_progress(1, 0.5, &current).await.unwrap();
    assert!((progress(&db).await - 0.5).abs() < 1e-9);
    // Unfenced work and other leases are not affected.
    store
        .update_progress(1, 0.75, &Fence::none("runner"))
        .await
        .unwrap();
    let other = Fence::advance(&t.pool, "other").await.unwrap();
    assert_eq!(other.epoch(), Some(1));
    current
        .check(&mut t.pool.acquire().await.unwrap())
        .await
        .unwrap();
    b.release().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successor_waits_for_a_fenced_write_in_progress() {
    let Some(t) = migrated().await else { return };
    let stale = Fence::advance(&t.pool, "runner").await.unwrap();
    let tx = stale.begin(&t.pool).await.unwrap();
    let pool = t.pool.clone();
    let mut advance = tokio::spawn(async move { Fence::advance(&pool, "runner").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut advance)
            .await
            .is_err(),
        "the successor's advance waits for the write that checked the old epoch"
    );
    tx.commit().await.unwrap();
    let current = tokio::time::timeout(Duration::from_secs(5), advance)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(current.epoch(), Some(2));
    assert!(stale.begin(&t.pool).await.unwrap_err().is_fenced());
}

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

/// Runs `run_singleton` for lease `name`, recording each fence it hands out.
fn contender(
    t: &TestDb,
    manager: &PgLeaseManager,
    name: &'static str,
    cancel: &CancellationToken,
) -> (Arc<Mutex<Vec<Fence>>>, tokio::task::JoinHandle<()>) {
    let fences = Arc::new(Mutex::new(Vec::new()));
    let backend = manager.lease(name);
    let (f, pool, c) = (fences.clone(), t.pool.clone(), cancel.clone());
    let handle = tokio::spawn(async move {
        run_singleton(
            name,
            backend,
            Some(pool),
            Duration::from_millis(200),
            move |token, fence| {
                f.lock().unwrap().push(fence);
                async move { token.cancelled().await }
            },
            c,
        )
        .await;
    });
    (fences, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_singleton_fences_the_leader_that_lost_its_lease() {
    let Some(t) = migrated().await else { return };
    let db = Db::from_pool(t.pool.clone());
    register_migration(&db).await;
    let ma = PgLeaseManager::from_pool(&t.pool).with_timeout(Duration::from_secs(2));
    let mb = PgLeaseManager::from_pool(&t.pool).with_timeout(Duration::from_secs(2));
    let cancel = CancellationToken::new();
    let (fa, ha) = contender(&t, &ma, "runner", &cancel);
    assert!(wait_for(Duration::from_secs(3), || fa.lock().unwrap().len() == 1).await);
    let (fb, hb) = contender(&t, &mb, "runner", &cancel);

    // A's lease connection dies (a network partition); B, or A on a new session, takes over.
    let pid = ma.backend_pid().await.unwrap();
    let _: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&t.pool)
        .await
        .unwrap();
    let terms = || fa.lock().unwrap().len() + fb.lock().unwrap().len();
    assert!(
        wait_for(Duration::from_secs(5), || terms() == 2).await,
        "a successor acquires"
    );
    let stale = fa.lock().unwrap()[0].clone();
    let current = fa
        .lock()
        .unwrap()
        .iter()
        .chain(fb.lock().unwrap().iter())
        .max_by_key(|f| f.epoch())
        .unwrap()
        .clone();
    assert_eq!(stale.epoch(), Some(1));
    assert_eq!(current.epoch(), Some(2));

    // A write A still had in flight is refused; B's goes through.
    let store = db.oob_migrations();
    let err = store.update_progress(1, 0.9, &stale).await.unwrap_err();
    assert!(err.is_fenced(), "{err}");
    store.update_progress(1, 0.5, &current).await.unwrap();
    assert!((progress(&db).await - 0.5).abs() < 1e-9);

    cancel.cancel();
    for h in [ha, hb] {
        tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .unwrap()
            .unwrap();
    }
}
