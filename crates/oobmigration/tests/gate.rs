//! The out-of-band migration gate against the test Postgres: an upgrade to a release
//! that deprecates an unfinished migration is refused (and changes nothing), startup of such a
//! release is refused, and both pass once the migration has completed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sourcegull_db::upgrade::{
    self, Catalog, OutOfBandGate, UpgradeError, UpgradeOptions, VersionPoint,
};
use sourcegull_db::{Db, Fence, MigrationDescriptor, MinorVersion, migrate};
use sourcegull_oobmigration::{
    Migrator, OutOfBandMigrationGate, Registry, check_startup, current_version, unfinished,
};
use sourcegull_testutil::TestDb;

/// A migration whose progress the test sets.
#[derive(Default)]
struct Fake {
    progress: Mutex<f64>,
}

impl Fake {
    fn set(&self, p: f64) {
        *self.progress.lock().unwrap() = p;
    }
}

#[async_trait]
impl Migrator for Fake {
    async fn progress(&self) -> anyhow::Result<f64> {
        Ok(*self.progress.lock().unwrap())
    }
    async fn up(&self, _fence: &Fence) -> anyhow::Result<()> {
        Ok(())
    }
    async fn down(&self, _fence: &Fence) -> anyhow::Result<()> {
        Ok(())
    }
}

fn descriptor(id: i32, deprecated: MinorVersion) -> MigrationDescriptor {
    MigrationDescriptor {
        id,
        team: "test",
        component: "test",
        description: "convert widgets",
        introduced: MinorVersion::new(0, 0),
        deprecated: Some(deprecated),
        non_destructive: true,
    }
}

#[tokio::test]
async fn upgrade_is_refused_until_deprecated_migrations_complete() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    let migrations = migrate::embedded();
    let old = migrations[20].0;
    let newest = migrations.last().unwrap().0;
    let point = |name: &str, last| VersionPoint {
        name: name.to_owned(),
        last_migration: last,
        description: None,
    };
    let catalog = Catalog::new(
        vec![point("0.1.0", old), point("0.2.0", newest)],
        migrations.clone(),
    );
    migrate::up_to(&t.pool, old).await.unwrap();

    let fake = Arc::new(Fake::default());
    fake.set(0.4);
    let mut registry = Registry::new();
    registry.register(descriptor(900, MinorVersion::new(0, 2)), fake.clone());
    let registry = Arc::new(registry);
    let gate = OutOfBandMigrationGate::new(registry.clone(), catalog.clone());

    // 0.1.0 → 0.2.0 is blocked (the table does not even exist yet: progress is measured).
    let err = upgrade::upgrade(
        &t.pool,
        &catalog,
        "0.1.0",
        "0.2.0",
        UpgradeOptions::default(),
        &gate,
    )
    .await
    .unwrap_err();
    match &err {
        UpgradeError::Gate { version, reason } => {
            assert_eq!(version, "0.2.0");
            assert!(reason.contains("out-of-band migration 900"), "{reason}");
            assert!(reason.contains("40% done"), "{reason}");
            assert!(reason.contains("must complete before 0.2"), "{reason}");
        }
        e => panic!("unexpected {e}"),
    }
    assert_eq!(upgrade::history(&t.pool).await.unwrap().newest, old);

    // An empty database has nothing to migrate.
    let empty_gate =
        OutOfBandMigrationGate::new(registry.clone(), catalog.clone()).empty_database(true);
    let step = catalog
        .plan(
            &catalog.resolve("0").unwrap(),
            &catalog.resolve("0.2.0").unwrap(),
        )
        .unwrap();
    for s in &step {
        empty_gate.before_step(&t.pool, s).await.unwrap();
    }

    // Bare migration numbers map to the release that contains them.
    let mid = catalog
        .plan(
            &catalog.resolve("0.1.0").unwrap(),
            &catalog.resolve(&migrations[25].0.to_string()).unwrap(),
        )
        .unwrap();
    assert_eq!(gate.step_version(&mid[0]), MinorVersion::new(0, 2));
    assert!(gate.before_step(&t.pool, &mid[0]).await.is_err());

    // Once complete, the upgrade goes through.
    fake.set(1.0);
    let report = upgrade::upgrade(
        &t.pool,
        &catalog,
        "0.1.0",
        "0.2.0",
        UpgradeOptions::default(),
        &gate,
    )
    .await
    .unwrap();
    assert!(report.applied);
    assert_eq!(upgrade::history(&t.pool).await.unwrap().newest, newest);

    // With the table in place, the stored row counts: applied in reverse it blocks even
    // though the data measures complete.
    let db = Db::from_pool(t.pool.clone());
    registry.sync(&db).await.unwrap();
    let row = db.oob_migrations().get(900).await.unwrap().unwrap();
    assert_eq!(row.progress, 1.0);
    assert!(row.complete());
    db.oob_migrations().set_direction(900, true).await.unwrap();
    let blocking = unfinished(&t.pool, &registry, MinorVersion::new(0, 2))
        .await
        .unwrap();
    assert_eq!(blocking.len(), 1);
    assert!(blocking[0].apply_reverse);
    assert!(
        unfinished(&t.pool, &registry, MinorVersion::new(0, 1))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn startup_is_refused_until_deprecated_migrations_complete() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    // Sourcegull's own registry has nothing deprecated yet: startup passes.
    let sourcegull = Registry::sourcegull(db.clone(), None);
    sourcegull.sync(&db).await.unwrap();
    check_startup(&db, &sourcegull).await.unwrap();
    let rows = db.oob_migrations().list().await.unwrap();
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        [
            sourcegull_oobmigration::REENCRYPT_SECRETS.id,
            sourcegull_oobmigration::ENCRYPT_MONITOR_WEBHOOKS.id
        ]
    );
    // Nothing to migrate in an empty database: registered complete.
    assert!(rows.iter().all(|r| r.progress == 1.0));

    let fake = Arc::new(Fake::default());
    let mut registry = Registry::new();
    registry.register(descriptor(901, current_version()), fake.clone());
    registry.sync(&db).await.unwrap();
    let err = check_startup(&db, &registry).await.unwrap_err().to_string();
    assert!(err.contains("out-of-band migration 901"), "{err}");
    assert!(err.contains("0% done"), "{err}");

    fake.set(1.0);
    check_startup(&db, &registry).await.unwrap();
    // The measured completion was stored.
    let row = db.oob_migrations().get(901).await.unwrap().unwrap();
    assert!(row.complete());
}
