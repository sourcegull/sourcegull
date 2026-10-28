//! The committed schema descriptions (`crates/db/schema/`) match the migrations.
//!
//! `current.json` describes the schema of every embedded migration; regenerate it with
//! `SOURCEGULL_UPDATE_SCHEMA=1 cargo test -p sourcegull-db --test integration schema_descriptions::` after adding a
//! migration. Each released version's file must describe the schema of its migrations, so a
//! released migration cannot be edited unnoticed.

use sourcegull_db::upgrade::{CURRENT_DESCRIPTION, CURRENT_VERSION, RELEASES};
use sourcegull_db::{migrate, schema};
use sourcegull_testutil::TestDb;

#[tokio::test]
async fn current_description_matches_the_migrations() {
    let Some(t) = TestDb::new().await else { return };
    migrate::up(&t.pool).await.unwrap();
    let json = schema::describe(&t.pool).await.unwrap().to_json();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/schema/current.json");
    if std::env::var_os("SOURCEGULL_UPDATE_SCHEMA").is_some() {
        std::fs::write(path, &json).expect("write crates/db/schema/current.json");
        return;
    }
    assert!(
        CURRENT_DESCRIPTION == json,
        "crates/db/schema/current.json is stale: run SOURCEGULL_UPDATE_SCHEMA=1 cargo test -p sourcegull-db --test integration schema_descriptions::"
    );
}

#[tokio::test]
async fn release_descriptions_match_their_migrations() {
    let newest = migrate::latest_version().unwrap();
    for (i, r) in RELEASES.iter().enumerate() {
        if let Some(next) = RELEASES.get(i + 1) {
            assert!(
                r.last_migration <= next.last_migration,
                "RELEASES must be oldest first"
            );
        }
        if r.version == CURRENT_VERSION {
            assert_eq!(
                r.last_migration, newest,
                "migrations were added after {CURRENT_VERSION} was released: bump the workspace version"
            );
        }
        let Some(t) = TestDb::new().await else { return };
        migrate::up_to(&t.pool, r.last_migration).await.unwrap();
        let actual = schema::describe(&t.pool).await.unwrap();
        let expected = schema::SchemaDescription::from_json(r.description).unwrap();
        let drift = schema::diff(&expected, &actual);
        assert!(
            drift.is_empty(),
            "the migrations up to {} no longer produce the schema released as {}:\n{}",
            r.last_migration,
            r.version,
            schema::report(&drift)
        );
    }
}
