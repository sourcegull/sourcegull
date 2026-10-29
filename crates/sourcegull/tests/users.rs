//! `sourcegull users reset-password` against the test database.

use std::time::Duration;

use clap::Parser as _;
use sourcegull::cli::{Cli, Command, UsersAction};
use sourcegull_db::{Db, LockoutPolicy, NewUser, migrate};
use sourcegull_testutil::TestDb;

#[tokio::test]
async fn reset_password_prints_a_redeemable_link_and_unlocks() {
    let Some(t) = TestDb::new().await else {
        return;
    };
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let user = db
        .users()
        .create(&NewUser {
            username: "Admin".into(),
            display_name: None,
            avatar_url: None,
            password_hash: None,
        })
        .await
        .unwrap();
    let policy = LockoutPolicy {
        max_failures: 1,
        window: Duration::from_secs(60),
        lockout: Duration::from_secs(3600),
    };
    db.users()
        .record_failed_login(user.id, "10.0.0.1", policy)
        .await
        .unwrap();
    assert!(
        db.users()
            .login_locked_until(user.id, "10.0.0.1")
            .await
            .unwrap()
            .is_some()
    );

    let cli = Cli::try_parse_from([
        "sourcegull",
        "users",
        "--database-url=x",
        "--external-url=https://sg.example.com/",
        "reset-password",
        "admin",
    ])
    .unwrap();
    let Command::Users(args) = cli.command else {
        panic!("not users")
    };
    assert_eq!(
        args.action,
        UsersAction::ResetPassword {
            username: "admin".into()
        }
    );
    let out = sourcegull::users_cmd::run(&db, &args).await.unwrap();
    let link = out.lines().next().unwrap();
    let id = sourcegull_services::relay::marshal_id("User", user.id.get());
    let prefix = format!("https://sg.example.com/reset-password?userID={id}&code=");
    let code = link
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{out}"));
    assert!(out.contains("valid for 4h"), "{out}");

    assert!(
        db.users()
            .login_locked_until(user.id, "10.0.0.1")
            .await
            .unwrap()
            .is_none()
    );
    let hash = sourcegull_crypto::session::hash_session_id(code);
    assert!(db.password_resets().redeem(user.id, &hash).await.unwrap());
    assert!(!db.password_resets().redeem(user.id, &hash).await.unwrap());

    let cli = Cli::try_parse_from([
        "sourcegull",
        "users",
        "--database-url=x",
        "reset-password",
        "nobody",
    ])
    .unwrap();
    let Command::Users(args) = cli.command else {
        panic!("not users")
    };
    let err = sourcegull::users_cmd::run(&db, &args).await.unwrap_err();
    assert!(err.to_string().contains("no active user"), "{err}");
}
