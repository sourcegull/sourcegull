//! The rollout window scheduler against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) with a
//! fake clock: rates, windows opening and closing, paused windows,
//! release order, removed windows.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use sourcegull_batches::store::{ChangesetState, ChangesetStore, NewChangeset};
use sourcegull_batches_reconciler::{FakeClock, SCHEDULER, Scheduler};
use sourcegull_config::{RolloutWindowConfig, SiteConfig, SiteConfigHandle};
use sourcegull_db::sqlx;
use sourcegull_db::{Db, Fence, migrate};
use sourcegull_testutil::TestDb;
use sourcegull_types::RepoId;

struct Env {
    _t: TestDb,
    db: Db,
    config: SiteConfigHandle,
    clock: FakeClock,
    scheduler: Scheduler,
    /// Scheduled changesets, oldest update first.
    ids: Vec<i64>,
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn windows(v: serde_json::Value) -> Vec<RolloutWindowConfig> {
    serde_json::from_value(v).unwrap()
}

/// `n` scheduled changesets and a scheduler at `now` (2026-10-19 is a Monday).
async fn setup(n: usize, rollout: serde_json::Value, now: &str) -> Option<Env> {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return None;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let repo: RepoId = sqlx::query_scalar(
        "INSERT INTO repo (name, external_id, external_service_type, external_service_id) \
         VALUES ('gitlab.example.com/a/one', '42', 'gitlab', 'https://gitlab.example.com/') \
         RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let c = ChangesetStore::new(&db)
            .create(&NewChangeset {
                scheduled: true,
                ..NewChangeset::new(repo, "gitlab")
            })
            .await
            .unwrap();
        // Created in reverse update order: the scheduler goes by `updated_at`.
        sqlx::query(
            "UPDATE changesets SET updated_at = now() - make_interval(mins => $2) WHERE id = $1",
        )
        .bind(c.id)
        .bind(i32::try_from(i).unwrap())
        .execute(db.pool())
        .await
        .unwrap();
        ids.push(c.id);
    }
    ids.reverse();
    let config = SiteConfigHandle::new(
        1,
        SiteConfig {
            batch_changes_rollout_windows: windows(rollout),
            ..SiteConfig::default()
        },
    );
    let clock = FakeClock::new(at(now));
    let scheduler = Scheduler::new(db.clone(), config.clone(), Arc::new(clock.clone()));
    Some(Env {
        _t: t,
        db,
        config,
        clock,
        scheduler,
        ids,
    })
}

impl Env {
    async fn tick(&mut self) -> (Vec<i64>, DateTime<Utc>) {
        let t = self.scheduler.tick().await.unwrap();
        (t.released, t.next)
    }

    async fn scheduled(&self) -> usize {
        ChangesetStore::new(&self.db)
            .scheduled_ids(100)
            .await
            .unwrap()
            .len()
    }
}

#[tokio::test]
async fn limited_rate_releases_evenly_in_update_order() {
    let Some(mut env) = setup(
        4,
        serde_json::json!([{"rate": "10/hour"}]),
        "2026-10-19T10:00:00Z",
    )
    .await
    else {
        return;
    };
    // A full bucket: one right away, the next in 6 minutes (polling meanwhile).
    let (released, next) = env.tick().await;
    assert_eq!(released, vec![env.ids[0]]);
    assert_eq!(next, at("2026-10-19T10:00:10Z"));
    let c = ChangesetStore::new(&env.db)
        .get(env.ids[0])
        .await
        .unwrap()
        .unwrap();
    assert!(c.window_released);
    assert_eq!(c.state, ChangesetState::Processing);
    // Too early: nothing, wake when the token is due.
    env.clock.set(at("2026-10-19T10:05:55Z"));
    let (released, next) = env.tick().await;
    assert!(released.is_empty());
    assert_eq!(next, at("2026-10-19T10:06:00Z"));
    env.clock.set(at("2026-10-19T10:06:00Z"));
    assert_eq!(env.tick().await.0, vec![env.ids[1]]);
    // Idle for an hour: no catching up, one at a time.
    env.clock.advance(Duration::hours(1));
    assert_eq!(env.tick().await.0, vec![env.ids[2]]);
    assert!(env.tick().await.0.is_empty());
    env.clock.advance(Duration::minutes(6));
    assert_eq!(env.tick().await.0, vec![env.ids[3]]);
    assert_eq!(env.scheduled().await, 0);
    // Nothing left: the token waits, the scheduler polls.
    env.clock.advance(Duration::minutes(6));
    let (released, next) = env.tick().await;
    assert!(released.is_empty());
    assert_eq!(next, env.clock_now() + Duration::seconds(10));
}

impl Env {
    fn clock_now(&self) -> DateTime<Utc> {
        use sourcegull_batches_reconciler::Clock;
        self.clock.now()
    }
}

#[tokio::test]
async fn windows_open_close_and_pause() {
    let Some(mut env) = setup(
        3,
        serde_json::json!([
            {"days": ["monday"], "start": "09:00", "end": "10:00", "rate": "2/hour"},
            {"days": ["monday"], "start": "09:30", "end": "09:45", "rate": 0},
        ]),
        "2026-10-19T08:00:00Z",
    )
    .await
    else {
        return;
    };
    // Before the window: nothing, wake at most every poll interval.
    let (released, next) = env.tick().await;
    assert!(released.is_empty());
    assert_eq!(next, at("2026-10-19T08:00:10Z"));
    // Shortly before the window opens: wake when it does.
    env.clock.set(at("2026-10-19T08:59:55Z"));
    assert_eq!(env.tick().await.1, at("2026-10-19T09:00:00Z"));
    // Open: one now, the next in 30 minutes, which falls in the paused window.
    env.clock.set(at("2026-10-19T09:00:00Z"));
    let (released, next) = env.tick().await;
    assert_eq!(released, vec![env.ids[0]]);
    assert_eq!(next, at("2026-10-19T09:00:10Z"));
    env.clock.set(at("2026-10-19T09:30:00Z"));
    let (released, next) = env.tick().await;
    assert!(released.is_empty());
    assert_eq!(next, at("2026-10-19T09:30:10Z"));
    // After the pause the first window's bucket starts again, full.
    env.clock.set(at("2026-10-19T09:45:00Z"));
    assert_eq!(env.tick().await.0, vec![env.ids[1]]);
    // Closed: the last one waits for next Monday.
    env.clock.set(at("2026-10-19T10:00:00Z"));
    assert!(env.tick().await.0.is_empty());
    env.clock.set(at("2026-10-26T09:00:00Z"));
    assert_eq!(env.tick().await.0, vec![env.ids[2]]);
}

#[tokio::test]
async fn unlimited_and_removed_windows_release_everything() {
    let Some(mut env) = setup(
        3,
        serde_json::json!([{"start": "12:00", "end": "13:00", "rate": "unlimited"}]),
        "2026-10-19T11:00:00Z",
    )
    .await
    else {
        return;
    };
    assert!(env.tick().await.0.is_empty());
    env.clock.set(at("2026-10-19T12:30:00Z"));
    let (mut released, next) = env.tick().await;
    released.sort_unstable();
    let mut all = env.ids.clone();
    all.sort_unstable();
    assert_eq!(released, all);
    assert_eq!(next, at("2026-10-19T12:30:10Z"));

    // Windows removed: scheduled changesets are released at once.
    ChangesetStore::new(&env.db)
        .schedule(&env.ids)
        .await
        .unwrap();
    assert!(env.config.store(
        2,
        SiteConfig {
            batch_changes_rollout_windows: Vec::new(),
            ..SiteConfig::default()
        },
    ));
    env.clock.set(at("2026-10-19T20:00:00Z"));
    let (released, _) = env.tick().await;
    assert_eq!(released.len(), 3);
    assert_eq!(env.scheduled().await, 0);
}

#[tokio::test]
async fn a_scheduler_that_lost_its_lease_releases_nothing() {
    let Some(env) = setup(3, serde_json::json!([]), "2026-10-19T10:00:00Z").await else {
        return;
    };
    let pool = env.db.pool().clone();
    let stale = Fence::advance(&pool, SCHEDULER).await.unwrap();
    let mut old = Scheduler::new(
        env.db.clone(),
        env.config.clone(),
        Arc::new(env.clock.clone()),
    )
    .with_fence(stale);
    let current = Fence::advance(&pool, SCHEDULER).await.unwrap();
    let err = old.tick().await.unwrap_err();
    assert!(
        matches!(&err, sourcegull_batches_reconciler::ReconcileError::Db(e) if e.is_fenced()),
        "{err}"
    );
    assert_eq!(env.scheduled().await, 3);

    let mut new = Scheduler::new(
        env.db.clone(),
        env.config.clone(),
        Arc::new(env.clock.clone()),
    )
    .with_fence(current);
    assert_eq!(new.tick().await.unwrap().released.len(), 3);
    assert_eq!(env.scheduled().await, 0);
}
