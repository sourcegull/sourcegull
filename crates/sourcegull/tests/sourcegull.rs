//! Tests for the CLI, `migrate` and the site config loader against the test database.

use std::time::Duration;

use clap::Parser as _;
use sourcegull::cli::{Cli, Command, MigrateAction};
use sourcegull::migrate_cmd;
use sourcegull::site_config::SiteConfigLoader;
use sourcegull::startup::{self, SchemaState};
use sourcegull_db::Db;
use sourcegull_testutil::TestDb;

#[test]
fn parses_the_command_line() {
    let cli = Cli::try_parse_from([
        "sourcegull",
        "migrate",
        "--database-url",
        "postgres://x",
        "down",
        "--to",
        "5",
    ])
    .unwrap();
    match cli.command {
        Command::Migrate(m) => assert_eq!(m.action, MigrateAction::Down { to: 5 }),
        other => panic!("unexpected {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "sourcegull",
        "run",
        "--roles=web,worker",
        "--database-url=postgres://x",
        "--http-addr=127.0.0.1:1234",
        "--repo-nodes=http://repo-0:3178,http://repo-1:3178",
    ])
    .unwrap();
    let Command::Run(run) = cli.command else {
        panic!("expected run")
    };
    assert_eq!(run.roles.to_string(), "web,worker");
    assert_eq!(run.server.http_addr.port(), 1234);
    assert_eq!(
        run.server.repo.repo_nodes,
        ["http://repo-0:3178", "http://repo-1:3178"]
    );
    assert_eq!(run.server.repo.repo_grpc_addr.port(), 3178);
    assert_eq!(run.server.repo.repo_node_addr, None);
    assert!(matches!(
        run.server.key_source().unwrap(),
        sourcegull::startup::KeySource::File { path, explicit: false }
            if path == std::path::Path::new("/var/opt/sourcegull/keys")
    ));

    assert!(
        Cli::try_parse_from(["sourcegull", "run", "--roles=nope", "--database-url=x"]).is_err()
    );
    assert!(Cli::try_parse_from(["sourcegull", "migrate", "--database-url=x", "down"]).is_err());
}

#[tokio::test]
async fn migrate_status_up_validate_down() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pool = &db.pool;

    let status = migrate_cmd::run(pool, MigrateAction::Status).await.unwrap();
    assert!(status.success);
    assert!(status.output.starts_with("VERSION"), "{}", status.output);
    assert!(status.output.contains("pending"), "{}", status.output);
    assert!(matches!(
        startup::schema_state(pool).await.unwrap(),
        SchemaState::Pending { .. }
    ));
    let err = startup::ensure_schema_current(pool).await.unwrap_err();
    assert!(err.to_string().contains("sourcegull migrate up"), "{err}");

    let up = migrate_cmd::run(pool, MigrateAction::Up).await.unwrap();
    assert!(up.output.starts_with("applied"), "{}", up.output);
    let again = migrate_cmd::run(pool, MigrateAction::Up).await.unwrap();
    assert!(again.output.contains("up to date"), "{}", again.output);
    startup::ensure_schema_current(pool).await.unwrap();

    let status = migrate_cmd::run(pool, MigrateAction::Status).await.unwrap();
    assert!(!status.output.contains("pending"), "{}", status.output);
    assert!(status.output.contains("applied"), "{}", status.output);

    let validate = migrate_cmd::run(pool, MigrateAction::Validate)
        .await
        .unwrap();
    assert!(validate.success, "{}", validate.output);

    let down = migrate_cmd::run(pool, MigrateAction::Down { to: 0 })
        .await
        .unwrap();
    assert!(down.output.contains("version 0"), "{}", down.output);
    assert!(matches!(
        startup::schema_state(pool).await.unwrap(),
        SchemaState::Pending { .. }
    ));
}

#[tokio::test]
async fn migrate_upgrade_drift_versions() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pool = &db.pool;
    let cli = Cli::try_parse_from([
        "sourcegull",
        "migrate",
        "--database-url=x",
        "upgrade",
        "--from=0",
        "--dry-run",
    ])
    .unwrap();
    let Command::Migrate(args) = cli.command else {
        panic!("not migrate")
    };
    let dry = migrate_cmd::run(pool, args.action).await.unwrap();
    assert!(dry.success, "{}", dry.output);
    assert!(dry.output.contains("(dry run)"), "{}", dry.output);
    assert!(
        dry.output.contains("drift check: skipped (empty database)"),
        "{}",
        dry.output
    );
    assert!(dry.output.contains("plan: "), "{}", dry.output);
    assert!(!dry.output.contains("applied"), "{}", dry.output);

    // An upgrade to a migration number, then on to this binary's version.
    let first = sourcegull_db::migrate::embedded()[10].0;
    let up = migrate_cmd::run(
        pool,
        MigrateAction::Upgrade {
            from: "0".to_owned(),
            to: Some(first.to_string()),
            dry_run: false,
            skip_drift_check: false,
        },
    )
    .await
    .unwrap();
    assert!(up.success, "{}", up.output);
    assert!(
        up.output.contains("applied 11 migration(s)"),
        "{}",
        up.output
    );
    assert!(up.output.contains("not checked"), "{}", up.output);
    let wrong = migrate_cmd::run(
        pool,
        MigrateAction::Upgrade {
            from: "0".to_owned(),
            to: None,
            dry_run: false,
            skip_drift_check: false,
        },
    )
    .await
    .unwrap_err();
    assert!(
        wrong.to_string().contains("not at empty database"),
        "{wrong}"
    );
    let rest = migrate_cmd::run(
        pool,
        MigrateAction::Upgrade {
            from: first.to_string(),
            to: None,
            dry_run: false,
            skip_drift_check: false,
        },
    )
    .await
    .unwrap();
    assert!(rest.success, "{}", rest.output);
    assert!(
        rest.output
            .contains("drift check: skipped (no schema description for migration"),
        "{}",
        rest.output
    );
    assert!(rest.output.ends_with(": ok"), "{}", rest.output);

    let versions = migrate_cmd::run(pool, MigrateAction::Versions)
        .await
        .unwrap();
    assert!(
        versions
            .output
            .contains(sourcegull_db::upgrade::CURRENT_VERSION),
        "{}",
        versions.output
    );
    assert!(
        versions
            .output
            .contains(&format!("({})", sourcegull_db::upgrade::CURRENT_VERSION)),
        "{}",
        versions.output
    );

    let drift = migrate_cmd::run(pool, MigrateAction::Drift { version: None })
        .await
        .unwrap();
    assert!(
        drift.success && drift.output.starts_with("no drift"),
        "{}",
        drift.output
    );
    sourcegull_db::sqlx::query("ALTER TABLE repo ADD COLUMN surprise int")
        .execute(pool)
        .await
        .unwrap();
    let drift = migrate_cmd::run(pool, MigrateAction::Drift { version: None })
        .await
        .unwrap();
    assert!(!drift.success);
    assert!(
        drift
            .output
            .contains("column \"repo.surprise\": unexpected")
            && drift
                .output
                .contains("ALTER TABLE repo DROP COLUMN surprise;"),
        "{}",
        drift.output
    );

    let described = migrate_cmd::run(pool, MigrateAction::Describe)
        .await
        .unwrap();
    assert!(described.output.contains("\"surprise\""));
}

async fn migrated() -> Option<(TestDb, Db)> {
    let test = TestDb::new().await?;
    sourcegull_db::migrate::up(&test.pool).await.unwrap();
    let db = Db::from_pool(test.pool.clone());
    Some((test, db))
}

#[tokio::test]
async fn site_config_first_boot_and_reload() {
    let Some((_test, db)) = migrated().await else {
        return;
    };
    let loader = SiteConfigLoader::load(&db, None, Some("https://env.example".into()))
        .await
        .unwrap();
    let handle = loader.handle();
    let first = db
        .site_config()
        .latest()
        .await
        .unwrap()
        .expect("default written");
    assert_eq!(
        first.contents,
        sourcegull_config::default_site_config_jsonc()
    );
    assert_eq!(handle.version(), first.id.get());
    // SOURCEGULL_EXTERNAL_URL fills in an unset externalURL.
    assert_eq!(
        handle.load().value.external_url.as_deref(),
        Some("https://env.example")
    );
    // A second process does not write another default.
    let other = SiteConfigLoader::load(&db, None, None).await.unwrap();
    assert_eq!(other.handle().version(), first.id.get());

    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(loader.run(cancel.clone()));
    let mut changes = handle.subscribe();

    // NOTIFY delivers the new version well before the 30 s poll.
    let second = db
        .site_config()
        .create(
            r#"{"externalURL": "https://db.example"}"#,
            None,
            Some(first.id),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.version() < second.id.get() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("reloaded via NOTIFY");
    assert_eq!(
        handle.load().value.external_url.as_deref(),
        Some("https://db.example")
    );

    // An invalid version is skipped and the previous config kept.
    let bad = db
        .site_config()
        .create(r#"{"externalURL": 42}"#, None, Some(second.id))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(handle.version(), second.id.get());
    assert!(bad.id.get() > second.id.get());

    // A stored SAML provider (rejected on save) is dropped at load; the rest applies.
    let saml = db
        .site_config()
        .create(
            r#"{"externalURL": "https://saml.example", "auth.providers": [
                {"type": "saml", "identityProviderMetadataURL": "https://idp/meta"},
                {"type": "builtin", "allowSignup": true}
            ]}"#,
            None,
            Some(bad.id),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.version() < saml.id.get() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("reloaded without the SAML provider");
    let loaded = handle.load();
    assert_eq!(
        loaded.value.external_url.as_deref(),
        Some("https://saml.example")
    );
    assert!(matches!(
        loaded.value.auth_providers.as_slice(),
        [sourcegull_config::AuthProvider::Builtin(b)] if b.allow_signup
    ));

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("reloader stops on cancel")
        .unwrap();
}

#[tokio::test]
async fn site_config_from_file() {
    let Some((_test, db)) = migrated().await else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("sourcegull-site-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("site.json");
    std::fs::write(
        &path,
        "{ // comment\n \"externalURL\": \"https://file.example\", }",
    )
    .unwrap();

    let mut loader = SiteConfigLoader::load(&db, Some(&path), None)
        .await
        .unwrap();
    assert!(loader.source().is_read_only());
    let handle = loader.handle();
    assert_eq!(handle.version(), 1);
    assert_eq!(
        handle.load().value.external_url.as_deref(),
        Some("https://file.example")
    );
    // File mode never writes to the database.
    assert!(db.site_config().latest().await.unwrap().is_none());

    assert!(!loader.reload().await.unwrap());
    std::fs::write(&path, r#"{"externalURL": "https://changed.example"}"#).unwrap();
    assert!(loader.reload().await.unwrap());
    assert_eq!(handle.version(), 2);

    std::fs::write(&path, r#"{"externalURL": 1}"#).unwrap();
    assert!(!loader.reload().await.unwrap());
    assert_eq!(handle.version(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
    // An invalid file at startup is an error.
    assert!(
        SiteConfigLoader::load(&db, Some(&path), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn site_config_file_resolves_secret_references() {
    let Some((_test, db)) = migrated().await else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("sourcegull-site-refs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("site.json");
    let secret = dir.join("url");
    std::fs::write(&secret, "https://secret.example\n").unwrap();
    std::fs::write(
        &path,
        format!(r#"{{"externalURL": "${{file:{}}}"}}"#, secret.display()),
    )
    .unwrap();

    let mut loader = SiteConfigLoader::load(&db, Some(&path), None)
        .await
        .unwrap();
    let handle = loader.handle();
    assert_eq!(
        handle.load().value.external_url.as_deref(),
        Some("https://secret.example")
    );
    assert!(!loader.reload().await.unwrap());
    // A changed secret file is a change, even though the config file is the same.
    std::fs::write(&secret, "https://rotated.example").unwrap();
    assert!(loader.reload().await.unwrap());
    assert_eq!(
        handle.load().value.external_url.as_deref(),
        Some("https://rotated.example")
    );

    // A missing reference keeps the previous config, then loads once it resolves.
    std::fs::remove_file(&secret).unwrap();
    assert!(!loader.reload().await.unwrap());
    assert!(!loader.reload().await.unwrap());
    std::fs::write(&secret, "https://back.example").unwrap();
    assert!(loader.reload().await.unwrap());

    std::fs::remove_file(&secret).unwrap();
    let err = format!(
        "{:#}",
        SiteConfigLoader::load(&db, Some(&path), None)
            .await
            .unwrap_err()
    );
    assert!(
        err.contains("/externalURL") && err.contains("cannot be read"),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_mentions_schema() {
    let text = sourcegull::version_text();
    assert!(text.starts_with("sourcegull "), "{text}");
    assert!(text.contains("schema version: 2026"), "{text}");
}

#[test]
fn parses_durations() {
    use sourcegull::cli::parse_duration;
    assert_eq!(parse_duration("5s"), Ok(Duration::from_secs(5)));
    assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
    assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
    assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
    assert_eq!(parse_duration("1.5"), Ok(Duration::from_millis(1500)));
    assert!(parse_duration("5h").is_err());
    assert!(parse_duration("-1s").is_err());
}

fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// The status of `GET path`, or `None` when the connection fails.
async fn status(addr: std::net::SocketAddr, path: &str) -> Option<u16> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
    s.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.ok()?;
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_fails_readyz_before_the_listeners_close() {
    let Some((test, db)) = migrated().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (http, internal) = (free_addr(), free_addr());
    let cli = Cli::try_parse_from([
        "sourcegull",
        "run",
        "--database-url",
        test.url.as_str(),
        "--roles",
        "web",
        "--http-addr",
        &http.to_string(),
        "--internal-addr",
        &internal.to_string(),
        "--data-dir",
        tmp.path().to_str().unwrap(),
        "--shutdown-delay",
        "3s",
    ])
    .unwrap();
    let Command::Run(args) = cli.command else {
        panic!("unexpected {:?}", cli.command)
    };
    assert_eq!(args.server.shutdown_delay, Duration::from_secs(3));
    let metrics = sourcegull_observability::install_metrics().unwrap();
    let signal = tokio_util::sync::CancellationToken::new();
    let running = tokio::spawn({
        let signal = signal.clone();
        async move {
            sourcegull::server::run_roles(&args.server, &args.roles, db, metrics, signal).await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while status(internal, "/readyz").await != Some(200) {
        assert!(tokio::time::Instant::now() < deadline, "never ready");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let stopped_at = tokio::time::Instant::now();
    signal.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(internal, "/readyz").await, Some(503));
    assert_eq!(status(http, "/readyz").await, Some(503));
    assert_eq!(status(http, "/healthz").await, Some(200));
    assert_eq!(status(internal, "/healthz").await, Some(200));

    tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("stops after the delay")
        .unwrap()
        .unwrap();
    assert!(stopped_at.elapsed() >= Duration::from_secs(3));
    assert_eq!(status(http, "/healthz").await, None);
}
