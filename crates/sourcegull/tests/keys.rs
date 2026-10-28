//! `sourcegull keys status|rotate` and the key reloader against the test database.

use std::sync::Arc;

use clap::Parser as _;
use sourcegull::cli::{Cli, Command, KeysAction};
use sourcegull::startup::KeySource;
use sourcegull_crypto::{RecordClass, Zeroizing};
use sourcegull_db::{Db, migrate};
use sourcegull_testutil::TestDb;

#[test]
fn parses_keys_commands() {
    let cli = Cli::try_parse_from([
        "sourcegull",
        "keys",
        "--database-url=x",
        "--keys-file=/k",
        "rotate",
    ])
    .unwrap();
    let Command::Keys(args) = cli.command else {
        panic!("not keys")
    };
    assert_eq!(args.action, KeysAction::Rotate);
}

#[tokio::test]
async fn keys_status_rotate_and_reload() {
    let Some(t) = TestDb::new().await else {
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let dir = std::env::temp_dir().join(format!("sourcegull-keys-cmd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("keys");
    sourcegull::startup::ensure_keys_file(&path).unwrap();

    // A keyring shared by a running process.
    let provider = Arc::new(sourcegull_crypto::FileKeyProvider::from_path(&path).unwrap());
    let shared = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let blob = shared.encrypt(RecordClass::OAuthToken, b"t").unwrap();
    let file = KeySource::File {
        path: path.clone(),
        explicit: true,
    };
    // The same keys given inline, as `SOURCEGULL_KEYS` would (comma-separated).
    let text = std::fs::read_to_string(&path).unwrap();
    let entries: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    let inline = KeySource::Inline(Arc::new(Zeroizing::new(entries.join(","))));

    let status = sourcegull::keys::run_command(&db, &inline, KeysAction::Status)
        .await
        .unwrap();
    assert!(
        status.contains("current key-encryption key: k1"),
        "{status}"
    );
    assert!(status.contains("oauth_token"), "{status}");
    assert!(status.contains("external_services.config_enc"), "{status}");

    let rotated = sourcegull::keys::run_command(&db, &file, KeysAction::Rotate)
        .await
        .unwrap();
    assert!(
        rotated.contains(&format!(
            "rotated: {} data key row(s)",
            2 * RecordClass::ALL.len()
        )),
        "{rotated}"
    );
    assert!(rotated.contains("retired"), "{rotated}");

    // The running process picks up the new keys.
    assert_eq!(shared.current_version(RecordClass::OAuthToken), Some(1));
    sourcegull::keys::reload(&db, &shared, &file).await;
    assert_eq!(shared.current_version(RecordClass::OAuthToken), Some(2));
    assert_eq!(
        shared
            .decrypt(RecordClass::OAuthToken, &blob)
            .unwrap()
            .as_slice(),
        b"t"
    );
    // Inline keys reload the data keys with the KEKs the process already has.
    sourcegull::keys::run_command(&db, &inline, KeysAction::Rotate)
        .await
        .unwrap();
    sourcegull::keys::reload(&db, &shared, &inline).await;
    assert_eq!(shared.current_version(RecordClass::OAuthToken), Some(3));
    std::fs::remove_dir_all(&dir).unwrap();
}
