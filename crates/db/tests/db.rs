//! Integration tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`).
//!
//! Tests use a multi-threaded runtime: `TestDb`'s `Drop` blocks the calling
//! thread while closing the pool, and sqlx returns connections to the pool on
//! spawned tasks that need another runtime thread to make progress.

use sourcegull_db::migrate::{self, MigrationProblem};
use sourcegull_db::{
    Db, DbError, DbOptions, ListUsersOptions, LockoutPolicy, NewSecurityEvent, NewUser,
};
use sourcegull_testutil::TestDb;
use sourcegull_types::{OrgRole, SettingsSubject, UserId};
use std::time::Duration;

async fn setup() -> Option<(TestDb, Db)> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate up");
    let db = Db::from_pool(t.pool.clone());
    Some((t, db))
}

async fn user(db: &Db, name: &str) -> UserId {
    db.users()
        .create(&NewUser {
            username: name.into(),
            ..Default::default()
        })
        .await
        .expect("create user")
        .id
}

async fn table_exists(db: &Db, name: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_applies_session_options() {
    let Some(t) = TestDb::new().await else { return };
    let opts = DbOptions {
        statement_timeout: Some(Duration::from_millis(1500)),
        application_name: "sourcegull-test".into(),
        max_connections: 2,
        ..Default::default()
    };
    let db = Db::connect(&t.url, &opts).await.unwrap();
    let (timeout, app): (String, String) = sqlx::query_as(
        "SELECT current_setting('statement_timeout'), current_setting('application_name')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(timeout, "1500ms");
    assert_eq!(app, "sourcegull-test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_round_trip_and_status() {
    let Some(t) = TestDb::new().await else { return };
    let pool = &t.pool;
    let latest = migrate::latest_version().unwrap();

    let before = migrate::status(pool).await.unwrap();
    assert!(!before.is_empty());
    assert!(
        before
            .iter()
            .all(|s| !s.applied && s.checksum_ok && s.known)
    );

    migrate::up(pool).await.unwrap();
    migrate::up(pool).await.unwrap(); // idempotent
    let after = migrate::status(pool).await.unwrap();
    assert!(
        after
            .iter()
            .all(|s| s.applied && s.checksum_ok && !s.dirty && s.applied_at.is_some())
    );
    assert!(migrate::validate(pool).await.unwrap().is_empty());

    // Seeds: USER has every default permission except OWNERSHIP.
    let user_perms: Vec<String> = sqlx::query_scalar(
        "SELECT p.namespace || '#' || p.action FROM role_permissions rp
         JOIN roles r ON r.id = rp.role_id JOIN permissions p ON p.id = rp.permission_id
         WHERE r.name = 'USER' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        user_perms,
        [
            "BATCH_CHANGES#READ",
            "BATCH_CHANGES#WRITE",
            "REPO_METADATA#WRITE"
        ]
    );

    // Revert only the newest migration.
    let second = after[after.len() - 2].version;
    migrate::down(pool, second).await.unwrap();
    let partial = migrate::status(pool).await.unwrap();
    assert!(!partial.last().unwrap().applied);
    assert!(partial[..partial.len() - 1].iter().all(|s| s.applied));

    // Revert everything: no tables left.
    migrate::down(pool, 0).await.unwrap();
    let db = Db::from_pool(pool.clone());
    for table in [
        "users",
        "roles",
        "site_config",
        "settings",
        "encryption_keys",
    ] {
        assert!(!table_exists(&db, table).await, "{table} should be dropped");
    }
    assert!(
        migrate::status(pool)
            .await
            .unwrap()
            .iter()
            .all(|s| !s.applied)
    );

    migrate::up(pool).await.unwrap();
    assert!(table_exists(&db, "users").await);
    assert_eq!(
        migrate::status(pool).await.unwrap().last().unwrap().version,
        latest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validate_detects_checksum_mismatch_and_unknown_versions() {
    let Some((t, _db)) = setup().await else {
        return;
    };
    let first = migrate::status(&t.pool).await.unwrap()[0].version;
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = $1")
        .bind(first)
        .execute(&t.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (99999999999999, 'from the future', true, '\\x00', 0)",
    )
    .execute(&t.pool)
    .await
    .unwrap();
    let problems = migrate::validate(&t.pool).await.unwrap();
    assert_eq!(
        problems,
        [
            MigrationProblem::ChecksumMismatch { version: first },
            MigrationProblem::UnknownApplied {
                version: 99999999999999
            },
        ]
    );
    let status = migrate::status(&t.pool).await.unwrap();
    let unknown = status.last().unwrap();
    assert!(!unknown.known && !unknown.checksum_ok && unknown.description == "from the future");
    // sqlx refuses to run on a modified history.
    assert!(matches!(
        migrate::up(&t.pool).await,
        Err(DbError::Migrate(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_store() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let users = db.users();

    let alice = users
        .create(&NewUser {
            username: "Alice".into(),
            display_name: Some("Alice A".into()),
            password_hash: Some("h1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !alice.site_admin,
        "first user is not auto site admin at this layer"
    );
    assert_eq!(alice.failed_logins, 0);

    let dup = users
        .create(&NewUser {
            username: "alice".into(),
            ..Default::default()
        })
        .await;
    assert!(
        matches!(dup, Err(DbError::AlreadyExists { ref constraint }) if constraint == "users_username_lower_key")
    );

    let bob = user(&db, "bob").await;
    assert_eq!(
        users.get_by_id(alice.id).await.unwrap().unwrap().username,
        "Alice"
    );
    assert_eq!(
        users.get_by_username("ALICE").await.unwrap().unwrap().id,
        alice.id
    );
    assert!(users.get_by_username("nobody").await.unwrap().is_none());
    assert_eq!(users.count().await.unwrap(), 2);
    assert_eq!(users.count_matching(None, false).await.unwrap(), 2);
    assert_eq!(users.count_matching(Some("LIC"), false).await.unwrap(), 1);
    assert_eq!(users.count_matching(Some("%"), false).await.unwrap(), 0);
    assert_eq!(users.count_site_admins().await.unwrap(), 0);

    let all = users.list(&ListUsersOptions::default()).await.unwrap();
    assert_eq!(
        all.iter().map(|u| u.id).collect::<Vec<_>>(),
        [alice.id, bob]
    );
    let q = ListUsersOptions {
        query: Some("ice a".into()),
        ..Default::default()
    };
    assert_eq!(users.list(&q).await.unwrap().len(), 1);
    let q = ListUsersOptions {
        query: Some("%".into()),
        ..Default::default()
    };
    assert!(
        users.list(&q).await.unwrap().is_empty(),
        "LIKE wildcards are escaped"
    );
    let page = ListUsersOptions {
        limit: 1,
        offset: 1,
        ..Default::default()
    };
    assert_eq!(users.list(&page).await.unwrap()[0].id, bob);

    // Roles: USER on create, SITE_ADMINISTRATOR follows the flag.
    let roles = |id: UserId| {
        let pool = db.pool().clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT r.name FROM user_roles ur JOIN roles r ON r.id = ur.role_id
                 WHERE ur.user_id = $1 ORDER BY r.name",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(roles(alice.id).await, ["USER"]);
    users.set_site_admin(alice.id, true).await.unwrap();
    assert!(users.get_by_id(alice.id).await.unwrap().unwrap().site_admin);
    assert_eq!(roles(alice.id).await, ["SITE_ADMINISTRATOR", "USER"]);
    users.set_site_admin(alice.id, false).await.unwrap();
    assert_eq!(roles(alice.id).await, ["USER"]);
    assert!(
        users
            .set_site_admin(UserId(9999), true)
            .await
            .unwrap_err()
            .is_not_found()
    );

    assert_eq!(
        users.password_hash(alice.id).await.unwrap().as_deref(),
        Some("h1")
    );
    users.set_password_hash(alice.id, Some("h2")).await.unwrap();
    assert_eq!(
        users.password_hash(alice.id).await.unwrap().as_deref(),
        Some("h2")
    );
    users.set_password_hash(alice.id, None).await.unwrap();
    assert_eq!(users.password_hash(alice.id).await.unwrap(), None);

    let policy = LockoutPolicy {
        max_failures: 3,
        window: Duration::from_secs(3600),
        lockout: Duration::from_secs(600),
    };
    const A: &str = "192.0.2.1";
    let f1 = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!((f1.failed_logins, f1.locked_until), (1, None));
    users.record_failed_login(bob, A, policy).await.unwrap();
    let f3 = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!(f3.failed_logins, 0);
    let locked = f3.locked_until.expect("locked after 3 failures");
    assert!(locked > chrono::Utc::now() + chrono::Duration::seconds(590));
    assert_eq!(
        users.login_locked_until(bob, A).await.unwrap(),
        Some(locked)
    );
    // Only for that address.
    assert_eq!(users.login_locked_until(bob, "").await.unwrap(), None);
    let other = users.record_failed_login(bob, "", policy).await.unwrap();
    assert_eq!((other.failed_logins, other.locked_until), (1, None));
    users.reset_failed_logins(bob).await.unwrap();
    assert_eq!(users.login_locked_until(bob, A).await.unwrap(), None);
    // A success from an address clears only its count.
    users.record_failed_login(bob, A, policy).await.unwrap();
    users.record_failed_login(bob, "", policy).await.unwrap();
    users.clear_failed_logins_from(bob, A).await.unwrap();
    let f = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!(f.failed_logins, 1);
    let f = users.record_failed_login(bob, "", policy).await.unwrap();
    assert_eq!(f.failed_logins, 2);
    users.reset_failed_logins(bob).await.unwrap();

    // Failures more than `window` after the first of the run start a new run.
    let backdate = |secs: i64| {
        let pool = db.pool().clone();
        async move {
            sqlx::query(
                "UPDATE sign_in_failures SET since = since - make_interval(secs => $2) WHERE user_id = $1",
            )
            .bind(bob)
            .bind(secs as f64)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let f1 = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!(f1.failed_logins, 1);
    backdate(3601).await;
    let f2 = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!((f2.failed_logins, f2.locked_until), (1, None));
    // Run anchored at the first failure: 1 inside, then a failure past the window.
    users.record_failed_login(bob, A, policy).await.unwrap();
    backdate(3601).await;
    let f = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!((f.failed_logins, f.locked_until), (1, None));
    // Three failures within the window lock.
    backdate(1800).await;
    users.record_failed_login(bob, A, policy).await.unwrap();
    let f3 = users.record_failed_login(bob, A, policy).await.unwrap();
    assert_eq!(f3.failed_logins, 0);
    assert!(f3.locked_until.is_some());
    users.reset_failed_logins(bob).await.unwrap();

    // Soft delete hides the user, frees the name and ends sessions.
    sqlx::query("INSERT INTO sessions (id_hash, user_id, expires_at) VALUES ('\\x01', $1, now() + interval '1 day')")
        .bind(bob)
        .execute(db.pool())
        .await
        .unwrap();
    users.soft_delete(bob).await.unwrap();
    assert!(users.get_by_id(bob).await.unwrap().is_none());
    assert!(users.soft_delete(bob).await.unwrap_err().is_not_found());
    assert_eq!(users.count().await.unwrap(), 1);
    let with_deleted = ListUsersOptions {
        include_deleted: true,
        ..Default::default()
    };
    assert_eq!(users.list(&with_deleted).await.unwrap().len(), 2);
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    user(&db, "bob").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_email_store() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let emails = db.user_emails();
    let a = user(&db, "a").await;
    let b = user(&db, "b").await;

    let first = emails.add(a, "a@example.com").await.unwrap();
    assert!(first.is_primary && !first.is_verified());
    let second = emails.add(a, "a2@example.com").await.unwrap();
    assert!(!second.is_primary);
    assert!(matches!(
        emails.add(a, "a@example.com").await,
        Err(DbError::AlreadyExists { .. })
    ));

    assert!(matches!(
        emails.set_primary(a, "a2@example.com").await,
        Err(DbError::Invalid(_))
    ));
    assert!(
        emails
            .verify(a, "missing@example.com")
            .await
            .unwrap_err()
            .is_not_found()
    );
    emails.verify(a, "a2@example.com").await.unwrap();
    emails.set_primary(a, "a2@example.com").await.unwrap();
    let list = emails.list(a).await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].email, "a2@example.com");
    assert!(list[0].is_primary && list[0].is_verified());
    assert!(!list[1].is_primary);

    assert_eq!(
        emails
            .user_by_verified_email("A2@EXAMPLE.COM")
            .await
            .unwrap(),
        Some(a)
    );
    assert_eq!(
        emails
            .user_by_verified_email("a@example.com")
            .await
            .unwrap(),
        None
    );

    // Another user may add the same address but not verify it.
    emails.add(b, "A2@example.com").await.unwrap();
    assert!(matches!(
        emails.verify(b, "A2@example.com").await,
        Err(DbError::AlreadyExists { .. })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_store() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let orgs = db.orgs();
    let u1 = user(&db, "u1").await;
    let u2 = user(&db, "u2").await;

    let org = orgs.create("acme", Some("Acme Inc")).await.unwrap();
    assert!(matches!(
        orgs.create("ACME", None).await,
        Err(DbError::AlreadyExists { .. })
    ));
    assert_eq!(
        orgs.get(org.id)
            .await
            .unwrap()
            .unwrap()
            .display_name
            .as_deref(),
        Some("Acme Inc")
    );
    assert_eq!(orgs.get_by_name("Acme").await.unwrap().unwrap().id, org.id);

    let m = orgs.add_member(org.id, u1, OrgRole::Admin).await.unwrap();
    assert_eq!(m.role, OrgRole::Admin);
    orgs.add_member(org.id, u2, OrgRole::Member).await.unwrap();
    assert!(matches!(
        orgs.add_member(org.id, u2, OrgRole::Member).await,
        Err(DbError::AlreadyExists { .. })
    ));
    let members = orgs.list_members(org.id).await.unwrap();
    assert_eq!(
        members
            .iter()
            .map(|m| (m.user_id, m.role))
            .collect::<Vec<_>>(),
        [(u1, OrgRole::Admin), (u2, OrgRole::Member)]
    );
    assert_eq!(orgs.list_for_user(u2).await.unwrap()[0].id, org.id);

    orgs.remove_member(org.id, u2).await.unwrap();
    assert!(
        orgs.remove_member(org.id, u2)
            .await
            .unwrap_err()
            .is_not_found()
    );
    db.users().soft_delete(u1).await.unwrap();
    assert!(orgs.list_members(org.id).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_store_listing_members_and_deletion() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let orgs = db.orgs();
    let u1 = user(&db, "carol").await;
    let u2 = user(&db, "dave").await;
    let a = orgs.create("acme", Some("Acme Inc")).await.unwrap();
    let b = orgs.create("beta", None).await.unwrap();
    let names = |v: Vec<sourcegull_types::Org>| v.into_iter().map(|o| o.name).collect::<Vec<_>>();
    assert_eq!(
        names(orgs.list(None, 10, 0).await.unwrap()),
        ["acme", "beta"]
    );
    assert_eq!(
        names(orgs.list(Some("INC"), 10, 0).await.unwrap()),
        ["acme"]
    );
    assert_eq!(names(orgs.list(None, 1, 1).await.unwrap()), ["beta"]);
    assert_eq!(orgs.count(Some("et")).await.unwrap(), 1);

    let a2 = orgs.update(a.id, None).await.unwrap();
    assert_eq!(a2.display_name, None);

    orgs.add_member(a.id, u1, OrgRole::Admin).await.unwrap();
    orgs.add_member(a.id, u2, OrgRole::Member).await.unwrap();
    assert_eq!(orgs.count_members(a.id, None).await.unwrap(), 2);
    let found = orgs.search_members(a.id, Some("DAV"), 10, 0).await.unwrap();
    assert_eq!(found.iter().map(|m| m.user_id).collect::<Vec<_>>(), [u2]);
    assert_eq!(
        orgs.get_member(a.id, u1).await.unwrap().unwrap().role,
        OrgRole::Admin
    );

    orgs.soft_delete(b.id).await.unwrap();
    assert!(orgs.soft_delete(b.id).await.unwrap_err().is_not_found());
    assert!(orgs.get(b.id).await.unwrap().is_none());
    assert_eq!(orgs.count(None).await.unwrap(), 1);
    assert!(
        orgs.update(b.id, Some("x"))
            .await
            .unwrap_err()
            .is_not_found()
    );
    // The name is free again.
    orgs.create("beta", None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_invitations() {
    use sourcegull_db::NewOrgInvitation;
    let Some((_t, db)) = setup().await else {
        return;
    };
    let orgs = db.orgs();
    let admin = user(&db, "admin").await;
    let erin = user(&db, "erin").await;
    let org = orgs.create("acme", None).await.unwrap();
    orgs.add_member(org.id, admin, OrgRole::Admin)
        .await
        .unwrap();

    let to_erin = orgs
        .create_invitation(&NewOrgInvitation {
            org_id: org.id,
            sender: admin,
            recipient_user: Some(erin),
            recipient_email: None,
            token_sha256: b"t1",
            expires_at: None,
        })
        .await
        .unwrap();
    assert!(to_erin.is_pending());
    let by_email = orgs
        .create_invitation(&NewOrgInvitation {
            org_id: org.id,
            sender: admin,
            recipient_user: None,
            recipient_email: Some("Erin@Example.com"),
            token_sha256: b"t2",
            expires_at: None,
        })
        .await
        .unwrap();
    let expired = orgs
        .create_invitation(&NewOrgInvitation {
            org_id: org.id,
            sender: admin,
            recipient_user: Some(erin),
            recipient_email: None,
            token_sha256: b"t3",
            expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        })
        .await
        .unwrap();
    assert!(!expired.is_pending());
    assert_eq!(orgs.pending_invitations(org.id).await.unwrap().len(), 2);
    let mine = orgs
        .pending_invitations_for_user(erin, &["erin@example.com".to_owned()])
        .await
        .unwrap();
    assert_eq!(
        mine.iter().map(|i| i.id).collect::<Vec<_>>(),
        [by_email.id, to_erin.id]
    );
    assert_eq!(
        orgs.get_invitation_by_token(b"t2")
            .await
            .unwrap()
            .unwrap()
            .id,
        by_email.id
    );

    // Declining does not add a member; an answered invitation cannot be answered again.
    let declined = orgs
        .respond_to_invitation(to_erin.id, erin, false)
        .await
        .unwrap();
    assert_eq!(declined.response_accepted, Some(false));
    assert!(!orgs.is_member(org.id, erin).await.unwrap());
    assert!(
        orgs.respond_to_invitation(to_erin.id, erin, true)
            .await
            .unwrap_err()
            .is_not_found()
    );
    // Accepting an email invitation records the recipient and adds the member.
    let accepted = orgs
        .respond_to_invitation(by_email.id, erin, true)
        .await
        .unwrap();
    assert_eq!(accepted.recipient_user_id, Some(erin));
    assert!(orgs.is_member(org.id, erin).await.unwrap());
    assert!(
        orgs.respond_to_invitation(expired.id, erin, true)
            .await
            .unwrap_err()
            .is_not_found()
    );

    // Revoking; deleting the org revokes the rest and hides its invitations.
    let again = orgs
        .create_invitation(&NewOrgInvitation {
            org_id: org.id,
            sender: admin,
            recipient_user: Some(erin),
            recipient_email: None,
            token_sha256: b"t4",
            expires_at: None,
        })
        .await
        .unwrap();
    orgs.revoke_invitation(again.id).await.unwrap();
    assert!(
        orgs.revoke_invitation(again.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(orgs.pending_invitations(org.id).await.unwrap().is_empty());
    let last = orgs
        .create_invitation(&NewOrgInvitation {
            org_id: org.id,
            sender: admin,
            recipient_user: Some(erin),
            recipient_email: None,
            token_sha256: b"t5",
            expires_at: None,
        })
        .await
        .unwrap();
    orgs.soft_delete(org.id).await.unwrap();
    assert!(orgs.get_invitation(last.id).await.unwrap().is_none());
    assert!(
        orgs.pending_invitations_for_user(erin, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn site_config_store_optimistic_concurrency() {
    let Some((t, db)) = setup().await else { return };
    let store = db.site_config();
    let admin = user(&db, "admin").await;
    assert!(store.latest().await.unwrap().is_none());

    let mut listener = sqlx::postgres::PgListener::connect_with(&t.pool)
        .await
        .unwrap();
    listener
        .listen(sourcegull_db::notify::channels::SITE_CONFIG)
        .await
        .unwrap();

    let v1 = store.create("{}", Some(admin), None).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.payload(), v1.id.to_string());

    let err = store.create("{\"a\":1}", None, None).await.unwrap_err();
    assert!(
        matches!(err, DbError::Conflict { expected: None, actual: Some(id) } if id == v1.id.get())
    );

    let v2 = store
        .create("// c\n{\"a\":1}", None, Some(v1.id))
        .await
        .unwrap();
    assert!(
        store
            .create("{}", None, Some(v1.id))
            .await
            .unwrap_err()
            .is_conflict()
    );
    let latest = store.latest().await.unwrap().unwrap();
    assert_eq!(latest, v2);
    assert_eq!(latest.contents, "// c\n{\"a\":1}");
    assert_eq!(v1.author_user_id, Some(admin));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_store_per_subject_versions() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.settings();
    let u = user(&db, "u").await;
    let org = db.orgs().create("o", None).await.unwrap().id;

    let g = store
        .create(SettingsSubject::Global, "{\"g\":1}", None, None)
        .await
        .unwrap();
    let us = store
        .create(SettingsSubject::User(u), "{\"u\":1}", Some(u), None)
        .await
        .unwrap();
    let os = store
        .create(SettingsSubject::Org(org), "{}", Some(u), None)
        .await
        .unwrap();
    assert_eq!(us.subject, SettingsSubject::User(u));

    // Versions are per subject: other subjects' writes don't conflict.
    let g2 = store
        .create(SettingsSubject::Global, "{\"g\":2}", None, Some(g.id))
        .await
        .unwrap();
    let err = store
        .create(SettingsSubject::Global, "{}", None, Some(g.id))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict { actual: Some(id), .. } if id == g2.id.get()));
    assert!(
        store
            .create(SettingsSubject::User(u), "{}", None, None)
            .await
            .unwrap_err()
            .is_conflict()
    );

    assert_eq!(
        store
            .latest(SettingsSubject::Global)
            .await
            .unwrap()
            .unwrap(),
        g2
    );
    assert_eq!(
        store
            .latest(SettingsSubject::User(u))
            .await
            .unwrap()
            .unwrap(),
        us
    );
    assert_eq!(
        store
            .latest(SettingsSubject::Org(org))
            .await
            .unwrap()
            .unwrap(),
        os
    );
    assert!(
        store
            .latest(SettingsSubject::User(UserId(424242)))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_event_store() {
    let Some((_t, db)) = setup().await else {
        return;
    };
    let store = db.security_events();
    let u = user(&db, "u").await;
    let e1 = store
        .insert(&NewSecurityEvent {
            name: "SignInFailed".into(),
            user_id: Some(u),
            ..Default::default()
        })
        .await
        .unwrap();
    let e2 = store
        .insert(&NewSecurityEvent {
            name: "SiteConfigUpdated".into(),
            source: "BACKEND".into(),
            argument: serde_json::json!({ "id": 2 }),
            ..Default::default()
        })
        .await
        .unwrap();
    let all = store.list_recent(None, 10).await.unwrap();
    assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), [e2, e1]);
    assert_eq!(all[0].argument, serde_json::json!({ "id": 2 }));
    assert_eq!(all[1].argument, serde_json::json!({}));
    let mine = store.list_recent(Some(u), 10).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].name, "SignInFailed");
    assert_eq!(store.list_recent(None, 1).await.unwrap().len(), 1);
}
