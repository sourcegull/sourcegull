//! `sourcegull migrate drift` and `upgrade --from --to` against Postgres.

use std::sync::Mutex;

use async_trait::async_trait;
use sourcegull_db::upgrade::{
    self, Catalog, OutOfBandGate, UpgradeError, UpgradeOptions, UpgradeStep, VersionPoint,
};
use sourcegull_db::{migrate, schema};
use sourcegull_testutil::TestDb;
use sqlx::PgPool;

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
async fn drift_is_detected_and_the_fix_sql_repairs_it() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let catalog = Catalog::embedded();
    let clean = upgrade::drift(&t.pool, &catalog, None).await.unwrap();
    assert_eq!(clean.version.last_migration, catalog.newest_migration());
    assert!(clean.drifts.is_empty(), "{}", schema::report(&clean.drifts));

    // A trigger to drop, wherever the schema has one.
    let live = schema::describe(&t.pool).await.unwrap();
    let (trigger_table, trigger) = live
        .tables
        .iter()
        .find_map(|(t, d)| d.triggers.keys().next().map(|n| (t.clone(), n.clone())))
        .expect("the schema has a trigger");
    exec(
        &t.pool,
        &format!(
            "ALTER TABLE repo DROP COLUMN blocked;
             ALTER TABLE repo ALTER COLUMN stars DROP DEFAULT;
             ALTER TABLE repo ALTER COLUMN stars TYPE bigint;
             ALTER TABLE repo ALTER COLUMN default_branch SET NOT NULL;
             ALTER TABLE repo DROP CONSTRAINT repo_name_check;
             DROP INDEX repo_created_at_idx;
             CREATE INDEX drift_extra_idx ON repo (updated_at);
             CREATE TABLE drift_scratch (id int);
             CREATE FUNCTION drift_extra() RETURNS int LANGUAGE sql AS 'SELECT 1';
             DROP TRIGGER {trigger} ON {trigger_table};"
        ),
    )
    .await;

    let report = upgrade::drift(
        &t.pool,
        &catalog,
        Some(&catalog.versions.last().unwrap().name),
    )
    .await
    .unwrap();
    let found: Vec<(String, String)> = report
        .drifts
        .iter()
        .map(|d| (d.object.clone(), d.problem.clone()))
        .collect();
    let has = |object: &str, problem: &str| {
        assert!(
            found
                .iter()
                .any(|(o, p)| o == object && p.starts_with(problem)),
            "{object}: {problem} not in\n{}",
            schema::report(&report.drifts)
        );
    };
    has("column \"repo.blocked\"", "missing");
    has("column \"repo.stars\"", "type differs");
    has("column \"repo.stars\"", "default differs");
    has("column \"repo.default_branch\"", "is NOT NULL");
    has("constraint \"repo_name_check\"", "missing");
    has("index \"repo_created_at_idx\"", "missing");
    has("index \"drift_extra_idx\"", "unexpected");
    has("table \"drift_scratch\"", "unexpected");
    has("function \"drift_extra()\"", "unexpected");
    has(&format!("trigger \"{trigger}\""), "missing");
    assert_eq!(
        report.drifts.len(),
        10,
        "{}",
        schema::report(&report.drifts)
    );

    // Running the suggested SQL removes the drift.
    for d in &report.drifts {
        for sql in &d.fix {
            exec(&t.pool, sql).await;
        }
    }
    let after = upgrade::drift(&t.pool, &catalog, None).await.unwrap();
    assert!(after.drifts.is_empty(), "{}", schema::report(&after.drifts));
}

/// Records the steps it is asked about and blocks before `block`.
#[derive(Default)]
struct RecordingGate {
    seen: Mutex<Vec<String>>,
    block: Option<String>,
}

#[async_trait]
impl OutOfBandGate for RecordingGate {
    async fn before_step(&self, _pool: &PgPool, step: &UpgradeStep) -> Result<(), String> {
        self.seen.lock().unwrap().push(step.version.clone());
        if self.block.as_deref() == Some(step.version.as_str()) {
            return Err("out-of-band migration 1 is 40% done".to_owned());
        }
        Ok(())
    }
}

async fn newest_applied(pool: &PgPool) -> i64 {
    upgrade::history(pool).await.unwrap().newest
}

#[tokio::test]
async fn upgrade_across_versions() {
    let Some(scratch) = TestDb::new().await else {
        return;
    };
    // Three versions over the real migrations: A, B and the current one, with descriptions
    // generated the way a release does.
    let migrations = migrate::embedded();
    let (a, b) = (migrations[4].0, migrations[20].0);
    let mut versions = Vec::new();
    for (name, last) in [("1.0.0", a), ("1.1.0", b)] {
        migrate::up_to(&scratch.pool, last).await.unwrap();
        versions.push(VersionPoint {
            name: name.to_owned(),
            last_migration: last,
            description: Some(schema::describe(&scratch.pool).await.unwrap().to_json()),
        });
    }
    versions.push(VersionPoint {
        name: "1.2.0".to_owned(),
        last_migration: migrations.last().unwrap().0,
        description: Some(upgrade::CURRENT_DESCRIPTION.to_owned()),
    });
    let catalog = Catalog::new(versions, migrations.clone());

    let Some(t) = TestDb::new().await else { return };
    let pool = &t.pool;
    migrate::up_to(pool, a).await.unwrap();
    let none = RecordingGate::default();
    let opts = UpgradeOptions::default();

    // A dry run plans and changes nothing.
    let dry = upgrade::upgrade(
        pool,
        &catalog,
        "1.0.0",
        "1.2.0",
        UpgradeOptions {
            dry_run: true,
            ..opts
        },
        &none,
    )
    .await
    .unwrap();
    assert!(dry.success() && !dry.applied);
    let plan: Vec<(&str, usize)> = dry
        .steps
        .iter()
        .map(|s| (s.version.as_str(), s.migrations.len()))
        .collect();
    assert_eq!(plan, [("1.1.0", 16), ("1.2.0", migrations.len() - 21)]);
    assert_eq!(newest_applied(pool).await, a);
    assert!(none.seen.lock().unwrap().is_empty());

    // The database must be at --from.
    let err = upgrade::upgrade(pool, &catalog, "1.1.0", "1.2.0", opts, &none)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not at 1.1.0"), "{err}");

    // Drift stops the upgrade before anything changes.
    exec(pool, "CREATE TABLE drift_scratch (id int)").await;
    let refused = upgrade::upgrade(pool, &catalog, "1.0.0", "1.2.0", opts, &none)
        .await
        .unwrap();
    assert!(!refused.success() && !refused.applied);
    assert_eq!(refused.drift_before.len(), 1);
    assert_eq!(newest_applied(pool).await, a);

    // --skip-drift-check goes ahead; an unfinished out-of-band migration stops it between
    // versions.
    let blocking = RecordingGate {
        block: Some("1.2.0".to_owned()),
        ..Default::default()
    };
    let err = upgrade::upgrade(
        pool,
        &catalog,
        "1.0.0",
        "1.2.0",
        UpgradeOptions {
            skip_drift_check: true,
            ..opts
        },
        &blocking,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, UpgradeError::Gate { ref version, .. } if version == "1.2.0"),
        "{err}"
    );
    assert_eq!(*blocking.seen.lock().unwrap(), ["1.1.0", "1.2.0"]);
    assert_eq!(newest_applied(pool).await, b);

    // Resume from 1.1.0 once the drift is repaired.
    exec(pool, "DROP TABLE drift_scratch").await;
    let gate = RecordingGate::default();
    let done = upgrade::upgrade(pool, &catalog, "v1.1.0", "1.2.0", opts, &gate)
        .await
        .unwrap();
    assert!(done.success() && done.applied, "{done:?}");
    assert!(done.drift_check_skipped.is_none());
    assert_eq!(done.drift_after.as_deref(), Some(&[][..]));
    assert_eq!(*gate.seen.lock().unwrap(), ["1.2.0"]);
    assert_eq!(newest_applied(pool).await, catalog.newest_migration());

    // Already there: nothing to do.
    let again = upgrade::upgrade(pool, &catalog, "1.2.0", "1.2.0", opts, &gate)
        .await
        .unwrap();
    assert!(again.steps.is_empty() && again.success());

    // The drift check infers the version from the history.
    let drift = upgrade::drift(pool, &catalog, None).await.unwrap();
    assert_eq!(drift.version.name, "1.2.0");
    assert!(drift.drifts.is_empty());
    // A bare migration number has no description.
    assert!(
        upgrade::drift(pool, &catalog, Some(&migrations[7].0.to_string()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn upgrade_from_an_empty_database() {
    let Some(t) = TestDb::new().await else { return };
    let catalog = Catalog::embedded();
    let to = catalog.versions.last().unwrap().name.clone();
    let report = upgrade::upgrade(
        &t.pool,
        &catalog,
        "0",
        &to,
        UpgradeOptions::default(),
        &upgrade::NoOutOfBandMigrations,
    )
    .await
    .unwrap();
    assert!(report.success() && report.applied);
    assert_eq!(
        report.drift_check_skipped.as_deref(),
        Some("empty database")
    );
    assert_eq!(report.drift_after.as_deref(), Some(&[][..]));
}
