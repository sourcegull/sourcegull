//! Key rotation end to end against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`):
//! secrets are written with key-encryption key k1, the key file gains k2 (rotation on
//! the next keyring load), the runner re-encrypts every encrypted column in batches, and
//! afterwards every secret is on the new data keys and readable with k2 alone.

use std::sync::Arc;

use sourcegull_batches::store::{BatchesCredentialStore, CredentialKind, CredentialSecret};
use sourcegull_crypto::{KeyProvider, KeyState, Keyring, RecordClass, StaticKeyProvider};
use sourcegull_db::{Db, ExecutorSecretScope, NewExternalService, SecretAccessor, migrate};
use sourcegull_oobmigration::reencrypt::{ENCRYPTED_COLUMNS, ReencryptSecrets};
use sourcegull_oobmigration::{Migrator, REENCRYPT_SECRETS, Registry, Runner};
use sourcegull_outbound_webhooks::store::{
    NewOutboundWebhook, OutboundWebhookStore, ScopedEventType,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::ExternalServiceKind;

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

/// Writes one secret of each kind; returns the ids to read them back.
async fn write_secrets(
    db: &Db,
    keyring: &Arc<Keyring>,
    tag: &str,
    n: usize,
) -> (Vec<i64>, i64, i64, i64) {
    let mut services = Vec::new();
    for i in 0..n {
        let s = db
            .external_services(Some(keyring.clone()))
            .create(&NewExternalService {
                kind: ExternalServiceKind::GitLab,
                display_name: format!("GitLab {tag}{i}"),
                config: format!(r#"{{"url":"https://gitlab{i}.example.com","token":"glpat-{i}"}}"#),
            })
            .await
            .unwrap();
        services.push(s.id.get());
    }
    let secret = db
        .executor_secrets(Some(keyring.clone()))
        .create(
            ExecutorSecretScope::Batches,
            &format!("TOKEN_{}", tag.to_uppercase()),
            "executor-value",
            None,
            None,
        )
        .await
        .unwrap();
    let credential = BatchesCredentialStore::new(db, Some(keyring.clone()))
        .set_site_credential(
            "gitlab",
            "https://gitlab.example.com/",
            &CredentialSecret {
                kind: CredentialKind::PersonalAccessToken,
                token: "glpat-site".into(),
                ssh: None,
            },
        )
        .await
        .unwrap();
    let hook = OutboundWebhookStore::new(db, Some(keyring.clone()))
        .create(&NewOutboundWebhook {
            url: format!("https://hooks.example.com/{tag}"),
            secret: "hook-secret".into(),
            event_types: vec![ScopedEventType {
                event_type: "batch_change:apply".into(),
                scope: None,
            }],
            created_by: None,
        })
        .await
        .unwrap();
    (services, secret.id, credential.id, hook.id)
}

#[tokio::test]
async fn rotation_reencrypts_every_secret() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());

    // Before any key exists, there is nothing to migrate.
    let probe = ReencryptSecrets::new(db.clone(), None);
    assert_eq!(probe.progress().await.unwrap(), 1.0);

    let ring1 = Arc::new(db.encryption_keys().load_keyring(k1()).await.unwrap());
    let (services, secret_id, credential_id, hook_id) = write_secrets(&db, &ring1, "a", 7).await;
    assert_eq!(probe.progress().await.unwrap(), 1.0);

    // The key file gains k2: the next load rotates (v1 retired and re-wrapped, v2 active).
    let (ring2, changed) = db
        .encryption_keys()
        .load_and_rotate(k1_k2(), sourcegull_crypto::RotateScope::KekChanged)
        .await
        .unwrap();
    assert_eq!(changed, 2 * RecordClass::ALL.len());
    let ring2 = Arc::new(ring2);
    let keys = db.encryption_keys().list().await.unwrap();
    assert!(keys.iter().all(|k| k.kek_id().unwrap() == "k2"));
    assert!(
        keys.iter()
            .all(|k| (k.version == 1) == (k.state == KeyState::Retired))
    );
    // Loading again rotates nothing.
    let (_, again) = db
        .encryption_keys()
        .load_and_rotate(k1_k2(), sourcegull_crypto::RotateScope::KekChanged)
        .await
        .unwrap();
    assert_eq!(again, 0);

    // The old process's keyring still encrypts with v1 until it reloads; its writes are
    // picked up by the migration too.
    write_secrets(&db, &ring1, "b", 1).await;
    let total: i64 = probe
        .counts()
        .await
        .unwrap()
        .iter()
        .map(|c| c.1.total)
        .sum();
    // 8 configs, 2 executor secrets, 1 site credential (replaced), 2 webhooks × 2 columns.
    assert_eq!(total, 8 + 2 + 1 + 4);
    assert!(probe.progress().await.unwrap() < 0.01);

    // The runner (with a keyring that does not know v2 yet: it reloads before each batch)
    // works through the secrets in batches and records progress.
    let stale_ring = Arc::new(Keyring::from_wrapped(k1_k2(), Vec::new()).unwrap());
    let registry = Arc::new(Registry::sourcegull(db.clone(), Some(stale_ring.clone())));
    registry.sync(&db).await.unwrap();
    let row = db
        .oob_migrations()
        .get(REENCRYPT_SECRETS.id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.progress < 0.01, "{row:?}");
    let runner = Runner::new(db.clone(), registry);
    let mut passes = 0;
    while runner.run_once().await.unwrap() {
        passes += 1;
        assert!(passes < 10, "migration does not finish");
    }
    assert!(passes >= 1);
    let row = db
        .oob_migrations()
        .get(REENCRYPT_SECRETS.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.progress, 1.0);
    assert!(row.complete());
    assert!(db.oob_migrations().errors(row.id).await.unwrap().is_empty());
    for (c, n) in probe.counts().await.unwrap() {
        assert_eq!(n.stale, 0, "{}.{}", c.table, c.column);
    }
    let versions: Vec<i32> = sqlx_versions(&db).await;
    assert!(versions.iter().all(|v| *v == 2), "{versions:?}");

    // Everything reads back with k2 alone (the retired v1 keys were re-wrapped with k2).
    let only_k2 = Arc::new(db.encryption_keys().load_keyring(k2_only()).await.unwrap());
    for (i, id) in services.iter().enumerate() {
        let config = db
            .external_services(Some(only_k2.clone()))
            .config(sourcegull_types::ExternalServiceId(*id))
            .await
            .unwrap();
        assert!(config.contains(&format!("glpat-{i}")), "{config}");
    }
    let es = db.executor_secrets(Some(only_k2.clone()));
    let secret = es
        .get(ExecutorSecretScope::Batches, secret_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        es.value(
            &secret,
            SecretAccessor::Machine {
                name: "test",
                job_id: None
            }
        )
        .await
        .unwrap(),
        "executor-value"
    );
    let creds = BatchesCredentialStore::new(&db, Some(only_k2.clone()));
    let site = creds
        .site_credential("gitlab", "https://gitlab.example.com/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(site.id, credential_id);
    assert_eq!(creds.secret(&site).await.unwrap().token, "glpat-site");
    let hook = OutboundWebhookStore::new(&db, Some(only_k2.clone()))
        .get(hook_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hook.secret, "hook-secret");
    drop(ring2);

    // Applied in reverse there is nothing to undo: progress reads 0 and data is untouched.
    db.oob_migrations()
        .set_direction(REENCRYPT_SECRETS.id, true)
        .await
        .unwrap();
    assert!(!runner.run_once().await.unwrap());
    let row = db
        .oob_migrations()
        .get(REENCRYPT_SECRETS.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.progress, 0.0);
    assert!(!row.complete());
    db.oob_migrations()
        .set_direction(REENCRYPT_SECRETS.id, false)
        .await
        .unwrap();
    assert!(!runner.run_once().await.unwrap());
    let row = db
        .oob_migrations()
        .get(REENCRYPT_SECRETS.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.progress, 1.0);
}

/// The `encryption_key_version` mirror columns of the credentials tables.
async fn sqlx_versions(db: &Db) -> Vec<i32> {
    sourcegull_db::sqlx::query_scalar(
        "SELECT encryption_key_version FROM site_credentials
         UNION ALL SELECT encryption_key_version FROM user_credentials",
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn runner_without_keys_records_errors() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let ring = Arc::new(db.encryption_keys().load_keyring(k1()).await.unwrap());
    write_secrets(&db, &ring, "a", 1).await;
    db.encryption_keys().rotate(k1()).await.unwrap();

    let registry = Arc::new(Registry::sourcegull(db.clone(), None));
    let runner = Runner::new(db.clone(), registry);
    registry_sync_and_pass(&db, &runner).await;
    let errors = db
        .oob_migrations()
        .errors(REENCRYPT_SECRETS.id)
        .await
        .unwrap();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].message.contains("SOURCEGULL_KEYS_FILE"),
        "{}",
        errors[0].message
    );
    let row = db
        .oob_migrations()
        .get(REENCRYPT_SECRETS.id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.progress < 1.0);
    // The newest 100 errors are kept.
    for i in 0..105 {
        db.oob_migrations()
            .add_error(REENCRYPT_SECRETS.id, &format!("e{i}"))
            .await
            .unwrap();
    }
    let errors = db
        .oob_migrations()
        .errors(REENCRYPT_SECRETS.id)
        .await
        .unwrap();
    assert_eq!(errors.len(), 100);
    assert_eq!(errors[0].message, "e104");
}

async fn registry_sync_and_pass(db: &Db, runner: &Runner) {
    Registry::sourcegull(db.clone(), None)
        .sync(db)
        .await
        .unwrap();
    // A failed batch backs off (not busy).
    assert!(!runner.run_once().await.unwrap());
}

#[test]
fn every_encrypted_column_has_a_class() {
    // Each class with encrypted columns appears; webhook secrets live inside connection
    // configs, so their class has no column of its own.
    for class in RecordClass::ALL {
        let listed = ENCRYPTED_COLUMNS.iter().any(|c| c.class == class);
        assert_eq!(
            listed,
            class != RecordClass::WebhookSecret,
            "{class} columns listed: {listed}"
        );
    }
}

#[tokio::test]
async fn every_bytea_enc_column_is_listed() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let columns: Vec<(String, String)> = sourcegull_db::sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns
         WHERE table_schema = 'public' AND data_type = 'bytea' AND column_name LIKE '%\\_enc'
         ORDER BY 1, 2",
    )
    .fetch_all(&t.pool)
    .await
    .unwrap();
    assert!(!columns.is_empty());
    for (table, column) in columns {
        assert!(
            ENCRYPTED_COLUMNS
                .iter()
                .any(|c| c.table == table && c.column == column),
            "{table}.{column} is encrypted but not re-encrypted after key rotations"
        );
    }
}
