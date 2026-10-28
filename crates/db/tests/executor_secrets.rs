//! Executor secret store tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`):
//! encryption round trip, uniqueness per namespace, namespace listings that overwrite global
//! secrets, and access logs.

use std::sync::Arc;

use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{
    Db, DbError, ExecutorSecretScope, ListExecutorSecrets, Namespace, NewUser, SecretAccessor,
    migrate,
};
use sourcegull_testutil::TestDb;

const BATCHES: ExecutorSecretScope = ExecutorSecretScope::Batches;
const CODEINTEL: ExecutorSecretScope = ExecutorSecretScope::CodeIntel;

#[tokio::test]
async fn secrets_round_trip_and_namespaces() {
    let Some(t) = TestDb::new().await else {
        eprintln!("skipping: SOURCEGULL_TEST_DATABASE_URL not set");
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let alice = db
        .users()
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let org = db.orgs().create("acme", None).await.unwrap().id;

    // Without a keyring nothing can be written.
    assert!(matches!(
        db.executor_secrets(None)
            .create(CODEINTEL, "TOKEN", "v", None, Some(alice))
            .await,
        Err(DbError::Invalid(_))
    ));

    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let store = db.executor_secrets(Some(keyring));

    let global = store
        .create(
            BATCHES,
            "GITHUB_TOKEN",
            "global-value-123",
            None,
            Some(alice),
        )
        .await
        .unwrap();
    assert_eq!(global.namespace, None);
    assert_eq!(global.creator_id, Some(alice));
    assert!(!global.overwrites_global_secret);
    let mine = store
        .create(
            BATCHES,
            "GITHUB_TOKEN",
            "alice-value-456",
            Some(Namespace::User(alice)),
            Some(alice),
        )
        .await
        .unwrap();
    assert!(mine.overwrites_global_secret);
    let other = store
        .create(BATCHES, "NPM_TOKEN", "npm-value-789", None, None)
        .await
        .unwrap();
    store
        .create(
            BATCHES,
            "ORG_ONLY",
            "org-value",
            Some(Namespace::Org(org)),
            None,
        )
        .await
        .unwrap();
    // Same key in another scope is a different secret.
    store
        .create(CODEINTEL, "GITHUB_TOKEN", "codeintel-value", None, None)
        .await
        .unwrap();

    // Unique per scope, key and namespace.
    let dup = store
        .create(BATCHES, "GITHUB_TOKEN", "again", None, None)
        .await;
    assert!(matches!(dup, Err(DbError::AlreadyExists { .. })), "{dup:?}");
    let dup = store
        .create(
            BATCHES,
            "GITHUB_TOKEN",
            "again",
            Some(Namespace::User(alice)),
            None,
        )
        .await;
    assert!(matches!(dup, Err(DbError::AlreadyExists { .. })));

    // The value is stored encrypted.
    let raw: Vec<u8> =
        sourcegull_db::sqlx::query_scalar("SELECT value_enc FROM executor_secrets WHERE id = $1")
            .bind(global.id)
            .fetch_one(&t.pool)
            .await
            .unwrap();
    assert!(!raw.windows(5).any(|w| w == b"globa"));

    // Listings: global only; the user's namespace with its own secret winning; the org's.
    let keys = |list: Vec<sourcegull_db::ExecutorSecret>| {
        list.into_iter()
            .map(|s| (s.key, s.namespace))
            .collect::<Vec<_>>()
    };
    let global_list = store
        .list(BATCHES, &ListExecutorSecrets::default())
        .await
        .unwrap();
    assert_eq!(
        keys(global_list),
        [("GITHUB_TOKEN".into(), None), ("NPM_TOKEN".into(), None)]
    );
    let user_opts = ListExecutorSecrets {
        namespace: Some(Namespace::User(alice)),
        ..Default::default()
    };
    assert_eq!(
        keys(store.list(BATCHES, &user_opts).await.unwrap()),
        [
            ("GITHUB_TOKEN".into(), Some(Namespace::User(alice))),
            ("NPM_TOKEN".into(), None)
        ]
    );
    assert_eq!(store.count(BATCHES, &user_opts).await.unwrap(), 2);
    let org_opts = ListExecutorSecrets {
        namespace: Some(Namespace::Org(org)),
        limit: 2,
        offset: 1,
        ..Default::default()
    };
    assert_eq!(
        keys(store.list(BATCHES, &org_opts).await.unwrap()),
        [
            ("NPM_TOKEN".into(), None),
            ("ORG_ONLY".into(), Some(Namespace::Org(org)))
        ]
    );
    let only = ListExecutorSecrets {
        keys: Some(vec!["GITHUB_TOKEN".into(), "MISSING".into()]),
        ..Default::default()
    };
    let found = store.list(CODEINTEL, &only).await.unwrap();
    assert_eq!(found.len(), 1);

    // Reading a value records who read it.
    assert_eq!(
        store
            .value(&global, SecretAccessor::User(alice))
            .await
            .unwrap(),
        "global-value-123"
    );
    assert_eq!(
        store
            .value(
                &found[0],
                SecretAccessor::Machine {
                    name: "codeintel-autoindexing",
                    job_id: Some(7)
                }
            )
            .await
            .unwrap(),
        "codeintel-value"
    );
    let logs = store.access_logs(global.id, 10, 0).await.unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].user_id, Some(alice));
    assert_eq!(logs[0].machine_user, "");
    let logs = store.access_logs(found[0].id, 10, 0).await.unwrap();
    assert_eq!(
        (
            logs[0].user_id,
            logs[0].machine_user.as_str(),
            logs[0].job_id
        ),
        (None, "codeintel-autoindexing", Some(7))
    );
    assert_eq!(store.count_access_logs(found[0].id).await.unwrap(), 1);

    // Update and delete, checked against the scope.
    assert!(
        store
            .update_value(CODEINTEL, other.id, "x")
            .await
            .unwrap()
            .is_none()
    );
    let updated = store
        .update_value(BATCHES, other.id, "npm-value-new")
        .await
        .unwrap()
        .unwrap();
    assert!(updated.updated_at >= other.updated_at);
    assert_eq!(
        store
            .value(&updated, SecretAccessor::User(alice))
            .await
            .unwrap(),
        "npm-value-new"
    );
    assert!(!store.delete(CODEINTEL, global.id).await.unwrap());
    assert!(store.delete(BATCHES, global.id).await.unwrap());
    assert!(store.get(BATCHES, global.id).await.unwrap().is_none());
    assert!(
        store
            .access_logs(global.id, 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
    // With the global secret gone, the user's no longer overwrites anything.
    let mine = store.get(BATCHES, mine.id).await.unwrap().unwrap();
    assert!(!mine.overwrites_global_secret);
}
