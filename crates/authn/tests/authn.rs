//! Integration tests against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`).

use sourcegull_authn::*;
use sourcegull_config::{
    AccessTokenAllow, AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig,
};
use sourcegull_db::{Db, migrate};
use sourcegull_testutil::TestDb;
use sourcegull_types::UserId;
use std::sync::Arc;
use std::time::Duration;

const PW: &str = "correct horse battery";

fn config(allow_signup: bool) -> SiteConfig {
    SiteConfig {
        auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider { allow_signup })],
        ..SiteConfig::default()
    }
}

struct Env {
    _t: TestDb,
    db: Db,
    cfg: ConfigHandle<SiteConfig>,
    auth: Authenticator,
}

impl Env {
    fn set(&self, f: impl FnOnce(&mut SiteConfig)) {
        let mut c = self.cfg.load().value.clone();
        f(&mut c);
        self.cfg.store(self.cfg.version() + 1, c);
    }
}

async fn setup_with(allow_signup: bool, pg_sessions: bool) -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let cfg = ConfigHandle::new(1, config(allow_signup));
    let sessions: Arc<dyn SessionStore> = if pg_sessions {
        Arc::new(PgSessionStore::new(db.clone()))
    } else {
        Arc::new(MemorySessionStore::new())
    };
    let auth = Authenticator::new(db.clone(), sessions, cfg.clone(), PasswordParams::TEST);
    Some(Env {
        _t: t,
        db,
        cfg,
        auth,
    })
}

async fn setup() -> Option<Env> {
    setup_with(true, true).await
}

async fn events(db: &Db, name: &str) -> usize {
    db.security_events()
        .list_recent(None, 1000)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.name == name)
        .count()
}

#[tokio::test]
async fn first_user_becomes_admin_second_does_not() {
    let Some(e) = setup().await else { return };
    assert!(e.auth.needs_site_init().await.unwrap());
    let a = e
        .auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    assert!(a.site_admin);
    let b = e.auth.sign_up("bob", "bob@example.com", PW).await.unwrap();
    assert!(!b.site_admin);
    assert!(!e.auth.needs_site_init().await.unwrap());
    let ae = e.db.user_emails().list(a.id).await.unwrap();
    assert!(ae[0].is_verified() && ae[0].is_primary);
    let be = e.db.user_emails().list(b.id).await.unwrap();
    assert!(!be[0].is_verified() && be[0].is_primary);

    let (_, s) = e.auth.sign_in("alice", PW, None).await.unwrap();
    let actor = e.auth.authenticate_session(&s).await.unwrap().unwrap();
    assert!(actor.site_admin);
    assert!(actor.has(Permission::OwnershipAssign));
    let (_, s) = e.auth.sign_in("bob", PW, None).await.unwrap();
    let actor = e.auth.authenticate_session(&s).await.unwrap().unwrap();
    assert!(!actor.site_admin);
    assert!(actor.has(Permission::BatchChangesWrite));
    assert!(!actor.has(Permission::OwnershipAssign));
    assert_eq!(events(&e.db, events::SIGN_UP_SUCCEEDED).await, 2);
}

#[tokio::test]
async fn signup_validation_and_conflicts() {
    let Some(e) = setup().await else { return };
    let code = |r: Result<_, AuthError>| r.unwrap_err().code();
    assert_eq!(
        code(e.auth.sign_up("-bad", "x@example.com", PW).await),
        "INVALID_USERNAME"
    );
    assert_eq!(
        code(e.auth.sign_up("ok", "nope", PW).await),
        "INVALID_EMAIL"
    );
    assert_eq!(
        code(e.auth.sign_up("ok", "x@example.com", "short").await),
        "WEAK_PASSWORD"
    );
    e.auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    assert_eq!(
        code(e.auth.sign_up("ALICE", "a2@example.com", PW).await),
        "USERNAME_TAKEN"
    );
    // alice's (first-user) email is verified, so it is taken.
    assert_eq!(
        code(e.auth.sign_up("carol", "Alice@example.com", PW).await),
        "EMAIL_TAKEN"
    );
    e.set(|c| {
        c.auth_password_policy.enabled = true;
        c.auth_password_policy.require_at_least_one_number = true;
    });
    assert_eq!(
        code(e.auth.sign_up("dave", "d@example.com", PW).await),
        "WEAK_PASSWORD"
    );
    e.auth
        .sign_up("dave", "d@example.com", "correct horse 1")
        .await
        .unwrap();
}

#[tokio::test]
async fn signup_disabled_except_for_first_user() {
    let Some(e) = setup_with(false, true).await else {
        return;
    };
    assert!(!e.auth.signup_allowed());
    let a = e
        .auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .unwrap();
    assert!(a.site_admin);
    let err = e
        .auth
        .sign_up("bob", "bob@example.com", PW)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "SIGNUP_DISABLED");
    e.set(|c| c.auth_providers.clear());
    let err = e.auth.sign_in("admin", PW, None).await.unwrap_err();
    assert_eq!(err.code(), "FORBIDDEN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_signups_yield_one_admin() {
    let Some(e) = setup_with(false, true).await else {
        return;
    };
    let mut handles = Vec::new();
    for i in 0..8 {
        let auth = e.auth.clone();
        handles.push(tokio::spawn(async move {
            auth.sign_up(&format!("user{i}"), &format!("u{i}@example.com"), PW)
                .await
        }));
    }
    let mut admins = 0;
    let mut ok = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(u) => {
                ok += 1;
                admins += usize::from(u.site_admin);
            }
            Err(err) => assert_eq!(err.code(), "SIGNUP_DISABLED"),
        }
    }
    assert_eq!((ok, admins), (1, 1));

    // With sign-up open, concurrent sign-ups on an empty instance still make exactly one admin.
    let Some(e) = setup_with(true, true).await else {
        return;
    };
    let mut handles = Vec::new();
    for i in 0..8 {
        let auth = e.auth.clone();
        handles.push(tokio::spawn(async move {
            auth.sign_up(&format!("user{i}"), &format!("u{i}@example.com"), PW)
                .await
        }));
    }
    let mut admins = 0;
    for h in handles {
        admins += usize::from(h.await.unwrap().unwrap().site_admin);
    }
    assert_eq!(admins, 1);
}

#[tokio::test]
async fn sign_in_by_username_or_verified_email() {
    let Some(e) = setup().await else { return };
    e.auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    e.auth.sign_up("bob", "bob@example.com", PW).await.unwrap();
    let ip = Some("10.0.0.1".parse().unwrap());
    assert_eq!(
        e.auth.sign_in("Alice", PW, ip).await.unwrap().0.username,
        "alice"
    );
    assert_eq!(
        e.auth
            .sign_in("alice@example.com", PW, ip)
            .await
            .unwrap()
            .0
            .username,
        "alice"
    );
    // bob's email is unverified.
    let err = e.auth.sign_in("bob@example.com", PW, ip).await.unwrap_err();
    assert_eq!(err.code(), "INVALID_CREDENTIALS");
    let err = e.auth.sign_in("nobody", PW, ip).await.unwrap_err();
    assert_eq!(err.code(), "INVALID_CREDENTIALS");
    let err = e
        .auth
        .sign_in("alice", "wrong password!", ip)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_CREDENTIALS");
    assert_eq!(events(&e.db, events::SIGN_IN_SUCCEEDED).await, 2);
    assert_eq!(events(&e.db, events::SIGN_IN_FAILED).await, 3);

    // Deleted users cannot sign in.
    let bob = e.db.users().get_by_username("bob").await.unwrap().unwrap();
    e.db.users().soft_delete(bob.id).await.unwrap();
    let err = e.auth.sign_in("bob", PW, None).await.unwrap_err();
    assert_eq!(err.code(), "INVALID_CREDENTIALS");
}

#[tokio::test]
async fn lockout_after_repeated_failures() {
    let Some(e) = setup().await else { return };
    e.set(|c| {
        c.auth_lockout.failed_attempt_threshold = 3;
        c.auth_lockout.lockout_period = 3600;
    });
    e.auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let stranger = Some("203.0.113.9".parse().unwrap());
    let owner = Some("198.51.100.7".parse().unwrap());
    for _ in 0..3 {
        let err = e
            .auth
            .sign_in("alice", "nope nope nope", stranger)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_CREDENTIALS");
    }
    assert_eq!(events(&e.db, events::ACCOUNT_LOCKED).await, 1);
    // Even the right password is refused from the locked address, with the same error as an
    // unknown account.
    let err = e.auth.sign_in("alice", PW, stranger).await.unwrap_err();
    assert_eq!(err.code(), "INVALID_CREDENTIALS");
    let unknown = e.auth.sign_in("nobody", PW, stranger).await.unwrap_err();
    assert_eq!(err.to_string(), unknown.to_string());
    // The owner, from another address, is not locked out.
    e.auth.sign_in("alice", PW, owner).await.unwrap();

    let id =
        e.db.users()
            .get_by_username("alice")
            .await
            .unwrap()
            .unwrap()
            .id;
    e.db.users().reset_failed_logins(id).await.unwrap();
    e.auth.sign_in("alice", PW, stranger).await.unwrap();
    // A success resets the address's counter.
    for _ in 0..2 {
        e.auth
            .sign_in("alice", "nope nope nope", stranger)
            .await
            .unwrap_err();
    }
    e.auth.sign_in("alice", PW, stranger).await.unwrap();
    for _ in 0..2 {
        e.auth
            .sign_in("alice", "nope nope nope", stranger)
            .await
            .unwrap_err();
    }
    e.auth.sign_in("alice", PW, stranger).await.unwrap();
}

#[test]
fn client_addresses_group_ipv6_by_network() {
    use sourcegull_authn::client_address;
    assert_eq!(client_address(None), "");
    assert_eq!(
        client_address(Some("192.0.2.1".parse().unwrap())),
        "192.0.2.1"
    );
    assert_eq!(
        client_address(Some("::ffff:192.0.2.1".parse().unwrap())),
        "192.0.2.1"
    );
    assert_eq!(
        client_address(Some("2001:db8:1:2:3:4:5:6".parse().unwrap())),
        "2001:db8:1:2::/64"
    );
}

async fn session_lifecycle_with(pg: bool) {
    let Some(e) = setup_with(true, pg).await else {
        return;
    };
    e.auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let (u, s1) = e.auth.sign_in("alice", PW, None).await.unwrap();
    let (_, s2) = e.auth.sign_in("alice", PW, None).await.unwrap();
    let actor = e.auth.authenticate_session(&s1).await.unwrap().unwrap();
    assert_eq!((actor.user_id, actor.via.clone()), (u.id, AuthVia::Session));
    assert!(
        e.auth
            .authenticate_session(&SessionToken::from("garbage"))
            .await
            .unwrap()
            .is_none()
    );
    e.auth.sign_out(&s1).await.unwrap();
    assert!(e.auth.authenticate_session(&s1).await.unwrap().is_none());
    assert!(e.auth.authenticate_session(&s2).await.unwrap().is_some());
    e.auth.sessions().delete_for_user(u.id).await.unwrap();
    assert!(e.auth.authenticate_session(&s2).await.unwrap().is_none());
    // Expired sessions are not returned.
    let s3 = e
        .auth
        .sessions()
        .create(u.id, Duration::ZERO)
        .await
        .unwrap();
    assert!(e.auth.sessions().get(&s3).await.unwrap().is_none());
    assert_eq!(events(&e.db, events::SIGN_OUT_SUCCEEDED).await, 1);
}

#[tokio::test]
async fn session_lifecycle_postgres() {
    session_lifecycle_with(true).await;
}

#[tokio::test]
async fn session_lifecycle_memory() {
    session_lifecycle_with(false).await;
}

#[tokio::test]
async fn pg_sessions_store_only_hashes_and_survive_cache_miss() {
    let Some(e) = setup().await else { return };
    let u = e
        .auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let s = e.auth.start_session(u.id).await.unwrap();
    let raw: Vec<Vec<u8>> = sqlx_rows(&e.db).await;
    assert_eq!(raw.len(), 1);
    assert_ne!(raw[0], s.as_str().as_bytes());
    assert_eq!(
        raw[0],
        sourcegull_crypto::session::hash_session_id(s.as_str()).to_vec()
    );
    // A second store (another process) reads it from Postgres.
    let other = PgSessionStore::new(e.db.clone());
    let got = other.get(&s).await.unwrap().unwrap();
    assert_eq!(got.user_id, u.id);
    other.delete(&s).await.unwrap();
    assert!(
        e.db.sessions()
            .get(&sourcegull_crypto::session::hash_session_id(s.as_str()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(other.delete_expired().await.unwrap(), 0);
}

async fn sqlx_rows(db: &Db) -> Vec<Vec<u8>> {
    sourcegull_db::sqlx::query_scalar("SELECT id_hash FROM sessions")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

fn scopes(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

async fn actor(e: &Env, name: &str) -> Actor {
    let (_, s) = e.auth.sign_in(name, PW, None).await.unwrap();
    e.auth.authenticate_session(&s).await.unwrap().unwrap()
}

#[tokio::test]
async fn access_token_lifecycle() {
    let Some(e) = setup().await else { return };
    e.auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .unwrap();
    let bob = e.auth.sign_up("bob", "bob@example.com", PW).await.unwrap();
    let bob_actor = actor(&e, "bob").await;
    let (id, token) = e
        .auth
        .create_access_token(&bob_actor, bob.id, &scopes(&[SCOPE_USER_ALL]), "ci", None)
        .await
        .unwrap();
    assert!(token.starts_with("skp_"));
    let a = e
        .auth
        .authenticate_access_token(&format!("token {token}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.user_id, bob.id);
    assert_eq!(
        a.via,
        AuthVia::Token {
            id,
            scopes: scopes(&[SCOPE_USER_ALL])
        }
    );
    assert!(a.has(Permission::BatchChangesRead));

    let listed = e.auth.list_access_tokens(&bob_actor, bob.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].note, "ci");
    assert!(listed[0].last_used_at.is_some());
    assert_eq!(listed[0].prefix, &token[4..12]);
    let exp = listed[0].expires_at.unwrap();
    let days = (exp - chrono::Utc::now()).num_days();
    assert!((364..=365).contains(&days), "{days}");
    // Debounced: a second use within a minute does not write.
    assert!(!e.db.access_tokens().touch_last_used(id).await.unwrap());

    // Not a token header → None; broken checksum / unknown token → TOKEN_INVALID.
    assert!(
        e.auth
            .authenticate_access_token("Basic abc")
            .await
            .unwrap()
            .is_none()
    );
    let mut bad = token.clone();
    bad.replace_range(10..11, if &token[10..11] == "a" { "b" } else { "a" });
    let err = e
        .auth
        .authenticate_access_token(&format!("token {bad}"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TOKEN_INVALID");
    let (unknown, _) = sourcegull_crypto::access_token::generate();
    let err = e
        .auth
        .authenticate_access_token(&format!("token {unknown}"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TOKEN_INVALID");

    // Another non-admin may not delete or list it.
    e.auth
        .sign_up("carol", "carol@example.com", PW)
        .await
        .unwrap();
    let carol = actor(&e, "carol").await;
    assert_eq!(
        e.auth
            .delete_access_token(&carol, id)
            .await
            .unwrap_err()
            .code(),
        "FORBIDDEN"
    );
    assert_eq!(
        e.auth
            .list_access_tokens(&carol, bob.id)
            .await
            .unwrap_err()
            .code(),
        "FORBIDDEN"
    );

    e.auth.delete_access_token(&bob_actor, id).await.unwrap();
    let err = e
        .auth
        .authenticate_access_token(&format!("token {token}"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TOKEN_INVALID");
    assert_eq!(
        e.auth
            .delete_access_token(&bob_actor, id)
            .await
            .unwrap_err()
            .code(),
        "NOT_FOUND"
    );
    assert_eq!(e.db.access_tokens().count(Some(bob.id)).await.unwrap(), 0);
    assert_eq!(events(&e.db, events::ACCESS_TOKEN_CREATED).await, 1);
    assert_eq!(events(&e.db, events::ACCESS_TOKEN_DELETED).await, 1);
}

#[tokio::test]
async fn access_token_expiry() {
    let Some(e) = setup().await else { return };
    let u = e
        .auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let a = actor(&e, "alice").await;
    let (_, token) = e
        .auth
        .create_access_token(
            &a,
            u.id,
            &scopes(&[SCOPE_USER_ALL]),
            "",
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();
    assert!(
        e.auth
            .authenticate_access_token(&format!("token {token}"))
            .await
            .unwrap()
            .is_some()
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let err = e
        .auth
        .authenticate_access_token(&format!("token {token}"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TOKEN_INVALID");
    // Over the maximum lifetime.
    e.set(|c| c.auth_access_tokens.max_token_expires_in_days = 7);
    let err = e
        .auth
        .create_access_token(
            &a,
            u.id,
            &scopes(&[SCOPE_USER_ALL]),
            "",
            Some(Duration::from_secs(8 * 86_400)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_INPUT");
}

#[tokio::test]
async fn access_token_policies() {
    let Some(e) = setup().await else { return };
    let admin_u = e
        .auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .unwrap();
    let bob = e.auth.sign_up("bob", "bob@example.com", PW).await.unwrap();
    let admin = actor(&e, "admin").await;
    let bob_a = actor(&e, "bob").await;
    let all = scopes(&[SCOPE_USER_ALL]);
    let code = |r: Result<_, AuthError>| r.unwrap_err().code();

    // Users only for themselves; admins for anyone.
    assert_eq!(
        code(
            e.auth
                .create_access_token(&bob_a, admin_u.id, &all, "", None)
                .await
        ),
        "FORBIDDEN"
    );
    e.auth
        .create_access_token(&admin, bob.id, &all, "", None)
        .await
        .unwrap();
    // Scope rules.
    assert_eq!(
        code(
            e.auth
                .create_access_token(&bob_a, bob.id, &scopes(&["repo:read"]), "", None)
                .await
        ),
        "INVALID_INPUT"
    );
    assert_eq!(
        code(
            e.auth
                .create_access_token(&bob_a, bob.id, &[], "", None)
                .await
        ),
        "INVALID_INPUT"
    );
    let sudo = scopes(&[SCOPE_USER_ALL, SCOPE_SITE_ADMIN_SUDO]);
    assert_eq!(
        code(
            e.auth
                .create_access_token(&bob_a, bob.id, &sudo, "", None)
                .await
        ),
        "FORBIDDEN"
    );

    // site-admin-create.
    e.set(|c| c.auth_access_tokens.allow = AccessTokenAllow::SiteAdminCreate);
    assert_eq!(
        code(
            e.auth
                .create_access_token(&bob_a, bob.id, &all, "", None)
                .await
        ),
        "FORBIDDEN"
    );
    let (_, t) = e
        .auth
        .create_access_token(&admin, bob.id, &all, "", None)
        .await
        .unwrap();
    // none: no creation, no use.
    e.set(|c| c.auth_access_tokens.allow = AccessTokenAllow::None);
    assert_eq!(
        code(
            e.auth
                .create_access_token(&admin, admin.user_id, &all, "", None)
                .await
        ),
        "TOKENS_DISABLED"
    );
    assert_eq!(
        e.auth
            .authenticate_access_token(&format!("token {t}"))
            .await
            .unwrap_err()
            .code(),
        "TOKENS_DISABLED"
    );
    e.set(|c| c.auth_access_tokens.allow = AccessTokenAllow::All);
    assert!(
        e.auth
            .authenticate_access_token(&format!("token {t}"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn token_sudo() {
    let Some(e) = setup().await else { return };
    let admin_u = e
        .auth
        .sign_up("admin", "admin@example.com", PW)
        .await
        .unwrap();
    let bob = e.auth.sign_up("bob", "bob@example.com", PW).await.unwrap();
    let admin = actor(&e, "admin").await;
    let sudo = scopes(&[SCOPE_USER_ALL, SCOPE_SITE_ADMIN_SUDO]);
    let (sid, st) = e
        .auth
        .create_access_token(&admin, admin_u.id, &sudo, "", None)
        .await
        .unwrap();
    let (_, plain) = e
        .auth
        .create_access_token(&admin, admin_u.id, &scopes(&[SCOPE_USER_ALL]), "", None)
        .await
        .unwrap();

    let a = e
        .auth
        .authenticate_access_token(&format!(r#"token-sudo user="bob",token="{st}""#))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.user_id, bob.id);
    assert!(!a.site_admin);
    assert_eq!(
        a.via,
        AuthVia::Sudo {
            token_id: sid,
            by_user_id: admin_u.id
        }
    );
    assert_eq!(events(&e.db, events::ACCESS_TOKEN_SUDO).await, 1);

    // A token without the sudo scope cannot sudo.
    let err = e
        .auth
        .authenticate_access_token(&format!(r#"token-sudo user="bob",token="{plain}""#))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "TOKEN_INVALID");
    // Unknown target user.
    let err = e
        .auth
        .authenticate_access_token(&format!(r#"token-sudo user="nobody",token="{st}""#))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
    // Once the subject is no longer a site admin, sudo stops working.
    e.db.users()
        .set_site_admin(admin_u.id, false)
        .await
        .unwrap();
    e.auth.invalidate_user(admin_u.id).await;
    let err = e
        .auth
        .authenticate_access_token(&format!(r#"token-sudo user="bob",token="{st}""#))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "FORBIDDEN");
}

#[tokio::test]
async fn rbac_store_assignments() {
    let Some(e) = setup().await else { return };
    let u = e
        .auth
        .sign_up("alice", "alice@example.com", PW)
        .await
        .unwrap();
    let rbac = e.db.rbac();
    let roles = rbac.list_roles().await.unwrap();
    let names: Vec<_> = roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["SITE_ADMINISTRATOR", "USER"]);
    let admin_role = roles[0].id;
    let user_role = roles[1].id;
    assert_eq!(rbac.roles_for_user(u.id).await.unwrap().len(), 2);
    assert!(rbac.unassign(u.id, admin_role).await.unwrap());
    assert!(!rbac.unassign(u.id, admin_role).await.unwrap());
    let perms = rbac.permissions_for_user(u.id).await.unwrap();
    assert!(!perms.contains(&("OWNERSHIP".into(), "ASSIGN".into())));
    assert_eq!(perms.len(), 3);
    rbac.assign(u.id, admin_role).await.unwrap();
    rbac.assign(u.id, admin_role).await.unwrap();
    assert_eq!(rbac.permissions_for_user(u.id).await.unwrap().len(), 4);
    assert!(rbac.unassign(u.id, user_role).await.unwrap());
    assert!(
        rbac.assign(u.id, sourcegull_types::RoleId(9999))
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        rbac.permissions_for_user(UserId(12345))
            .await
            .unwrap()
            .is_empty()
    );
}
