//! Out-of-band migration 2 against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`):
//! watch webhook URLs and email headers stored in plaintext are encrypted once keys exist, a key
//! rotation re-encrypts them like every other secret, and applying the migration in reverse
//! restores the plaintext for a downgrade.

use std::sync::Arc;

use sourcegull_crypto::{KeyProvider, Keyring, RecordClass, RotateScope, StaticKeyProvider};
use sourcegull_db::{Db, Fence, NewUser, migrate};
use sourcegull_oobmigration::monitor_webhooks::EncryptMonitorWebhooks;
use sourcegull_oobmigration::reencrypt::ReencryptSecrets;
use sourcegull_oobmigration::{
    ENCRYPT_MONITOR_WEBHOOKS, Migrator, OOB_MIGRATION_RUNNER, Registry, Runner,
};
use sourcegull_testutil::TestDb;

const HOOK: &str = "https://hooks.example.com/token-123";
const SLACK: &str = "https://hooks.slack.com/services/T0/B0/xyz";
const HEADER: &str = "Approved: list-password";

fn k1() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new("k1", [1; 32]).unwrap())
}

fn k1_k2() -> Arc<dyn KeyProvider> {
    Arc::new(
        StaticKeyProvider::new("k1", [1; 32])
            .and_then(|p| p.with_key("k2", [2; 32]))
            .unwrap(),
    )
}

fn k2_only() -> Arc<dyn KeyProvider> {
    Arc::new(StaticKeyProvider::new("k2", [2; 32]).unwrap())
}

/// `(kind, url, url_enc)` of every action, by id.
async fn actions(db: &Db) -> Vec<(String, String, Option<Vec<u8>>)> {
    sourcegull_db::sqlx::query_as("SELECT kind, url, url_enc FROM cm_actions ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// `(header, header_enc)` of every action, by id.
async fn headers(db: &Db) -> Vec<(String, Option<Vec<u8>>)> {
    sourcegull_db::sqlx::query_as("SELECT header, header_enc FROM cm_actions ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn run_until_idle(runner: &Runner) {
    let mut passes = 0;
    while runner.run_once().await.unwrap() {
        passes += 1;
        assert!(passes < 10, "migration does not finish");
    }
}

async fn progress(db: &Db) -> f64 {
    db.oob_migrations()
        .get(ENCRYPT_MONITOR_WEBHOOKS.id)
        .await
        .unwrap()
        .unwrap()
        .progress
}

fn decrypt(ring: &Keyring, blob: &Option<Vec<u8>>) -> String {
    let plain = ring
        .decrypt(RecordClass::CodeMonitorWebhook, blob.as_ref().unwrap())
        .unwrap();
    String::from_utf8(plain.to_vec()).unwrap()
}

#[tokio::test]
async fn plaintext_webhook_urls_are_encrypted() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    // Actions written before encryption (or without keys): plaintext URLs.
    let user = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let monitor: i64 = sourcegull_db::sqlx::query_scalar(
        "INSERT INTO cm_monitors (description, namespace_user_id, query)
         VALUES ('m', $1, 'type:diff TODO') RETURNING id",
    )
    .bind(user)
    .fetch_one(db.pool())
    .await
    .unwrap();
    sourcegull_db::sqlx::query(
        "INSERT INTO cm_actions (monitor_id, kind, url, header) VALUES
             ($1, 'webhook', $2, ''), ($1, 'slack_webhook', $3, ''), ($1, 'email', '', $4)",
    )
    .bind(monitor)
    .bind(HOOK)
    .bind(SLACK)
    .bind(HEADER)
    .execute(db.pool())
    .await
    .unwrap();

    // Without keys, plaintext is the expected format: nothing to do.
    let probe = EncryptMonitorWebhooks::new(db.clone(), None);
    assert_eq!(probe.counts().await.unwrap(), (0, 3));
    assert_eq!(probe.progress().await.unwrap(), 1.0);

    // Keys are configured: the URLs need encrypting, and the runner does it.
    let ring = Arc::new(db.encryption_keys().load_keyring(k1()).await.unwrap());
    assert_eq!(probe.progress().await.unwrap(), 0.0);
    assert!(probe.up(&Fence::none("test")).await.is_err(), "no keyring");
    let registry = Arc::new(Registry::sourcegull(db.clone(), Some(ring.clone())));
    registry.sync(&db).await.unwrap();
    assert_eq!(progress(&db).await, 0.0);
    // A runner that lost its lease migrates nothing and records nothing.
    let stale = Fence::advance(db.pool(), OOB_MIGRATION_RUNNER)
        .await
        .unwrap();
    let stale_runner = Runner::new(db.clone(), registry.clone()).with_fence(stale);
    Fence::advance(db.pool(), OOB_MIGRATION_RUNNER)
        .await
        .unwrap();
    let m = registry
        .iter()
        .find(|m| m.descriptor.id == ENCRYPT_MONITOR_WEBHOOKS.id)
        .unwrap();
    assert!(stale_runner.step(m).await.unwrap_err().is_fenced());
    assert_eq!(probe.counts().await.unwrap(), (0, 3));
    assert!(
        db.oob_migrations()
            .errors(ENCRYPT_MONITOR_WEBHOOKS.id)
            .await
            .unwrap()
            .is_empty()
    );
    let runner = Runner::new(db.clone(), registry);
    run_until_idle(&runner).await;
    assert_eq!(progress(&db).await, 1.0);
    assert!(
        db.oob_migrations()
            .errors(ENCRYPT_MONITOR_WEBHOOKS.id)
            .await
            .unwrap()
            .is_empty()
    );
    let rows = actions(&db).await;
    assert_eq!(rows[0].1, "");
    assert_eq!(decrypt(&ring, &rows[0].2), HOOK);
    assert_eq!(rows[1].1, "");
    assert_eq!(decrypt(&ring, &rows[1].2), SLACK);
    assert_eq!(rows[2], ("email".to_owned(), String::new(), None));
    let h = headers(&db).await;
    assert_eq!(h[0], (String::new(), None));
    assert_eq!(h[2].0, "");
    assert_eq!(decrypt(&ring, &h[2].1), HEADER);

    // A rotation re-encrypts them like every other secret; afterwards k2 alone reads them.
    let (ring2, _) = db
        .encryption_keys()
        .load_and_rotate(k1_k2(), RotateScope::KekChanged)
        .await
        .unwrap();
    let ring2 = Arc::new(ring2);
    let reencrypt = ReencryptSecrets::new(db.clone(), None);
    let stale: i64 = reencrypt
        .counts()
        .await
        .unwrap()
        .iter()
        .filter(|(c, _)| c.table == "cm_actions")
        .map(|(_, n)| n.stale)
        .sum();
    assert_eq!(stale, 3);
    let runner = Runner::new(
        db.clone(),
        Arc::new(Registry::sourcegull(db.clone(), Some(ring2.clone()))),
    );
    run_until_idle(&runner).await;
    assert_eq!(reencrypt.progress().await.unwrap(), 1.0);
    let k2_ring = db.encryption_keys().load_keyring(k2_only()).await.unwrap();
    let rows = actions(&db).await;
    assert_eq!(decrypt(&k2_ring, &rows[0].2), HOOK);
    assert_eq!(decrypt(&k2_ring, &rows[1].2), SLACK);
    assert_eq!(decrypt(&k2_ring, &headers(&db).await[2].1), HEADER);

    // Applied in reverse (before a downgrade): plaintext again.
    db.oob_migrations()
        .set_direction(ENCRYPT_MONITOR_WEBHOOKS.id, true)
        .await
        .unwrap();
    run_until_idle(&runner).await;
    assert_eq!(progress(&db).await, 0.0);
    let rows = actions(&db).await;
    assert_eq!(rows[0], ("webhook".to_owned(), HOOK.to_owned(), None));
    assert_eq!(
        rows[1],
        ("slack_webhook".to_owned(), SLACK.to_owned(), None)
    );
    assert_eq!(headers(&db).await[2], (HEADER.to_owned(), None));
    // Nothing left to decrypt: further passes are idle.
    assert!(!runner.run_once().await.unwrap());

    // And forward again.
    db.oob_migrations()
        .set_direction(ENCRYPT_MONITOR_WEBHOOKS.id, false)
        .await
        .unwrap();
    run_until_idle(&runner).await;
    assert_eq!(progress(&db).await, 1.0);
    assert_eq!(decrypt(&ring2, &actions(&db).await[0].2), HOOK);
}

#[tokio::test]
async fn nothing_to_migrate_is_idle_both_ways() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let ring = Arc::new(db.encryption_keys().load_keyring(k1()).await.unwrap());
    let registry = Arc::new(Registry::sourcegull(db.clone(), Some(ring)));
    registry.sync(&db).await.unwrap();
    let runner = Runner::new(db.clone(), registry);
    assert!(!runner.run_once().await.unwrap());
    assert_eq!(progress(&db).await, 1.0);
    // In reverse with no data, progress stays 1 but the runner does not spin.
    db.oob_migrations()
        .set_direction(ENCRYPT_MONITOR_WEBHOOKS.id, true)
        .await
        .unwrap();
    assert!(!runner.run_once().await.unwrap());
}
