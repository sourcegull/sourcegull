//! Sign-up / sign-in / sign-out cookie flow, auth-state, token auth and the CSRF rule.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull_authn::{AuthVia, Authenticator, PasswordParams, PgSessionStore};
use sourcegull_config::HttpHeaderAuthProvider;
use sourcegull_config::{AllowedIpAddress, SmtpAuthentication, SmtpConfig, SmtpEncryption};
use sourcegull_config::{AuthProvider, BuiltinAuthProvider, ConfigHandle, SiteConfig};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::{auth_routes, finish, public_routes};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const PW: &str = "correct horse battery";

struct Env {
    _t: TestDb,
    db: Db,
    router: Router,
    auth: Authenticator,
    config: ConfigHandle<SiteConfig>,
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let config = ConfigHandle::new(
        1,
        SiteConfig {
            auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider {
                allow_signup: false,
            })],
            ..SiteConfig::default()
        },
    );
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = MetricsHandle::from_prometheus(recorder.handle());
    let state = AppState::new(db.clone(), config.clone(), metrics, auth.clone());
    let router = finish(public_routes().merge(auth_routes()), state);
    Some(Env {
        _t: t,
        db,
        router,
        auth,
        config,
    })
}

struct Resp {
    status: StatusCode,
    set_cookie: Option<String>,
    location: Option<String>,
    body: Value,
}

impl Env {
    async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned());
        let location = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            set_cookie,
            location,
            body,
        }
    }

    async fn post(&self, path: &str, body: Value, cookie: Option<&str>, csrf: bool) -> Resp {
        let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        if csrf {
            req = req.header("X-Requested-With", "Sourcegull");
        }
        self.send(req.body(Body::from(body.to_string())).unwrap())
            .await
    }

    async fn auth_state(&self, cookie: Option<&str>) -> Value {
        let mut req = Request::get("/-/auth-state");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let r = self.send(req.body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::OK);
        r.body
    }
}

/// `sourcegull_session=<value>` from a `Set-Cookie` header.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

#[tokio::test]
async fn verification_and_reset_links_are_single_use() {
    let Some(mut env) = setup().await else { return };
    let mut config = env.config.load().value.clone();
    config.external_url = Some("https://sourcegull.example".into());
    config.auth_providers = vec![AuthProvider::Builtin(BuiltinAuthProvider {
        allow_signup: true,
    })];
    config.email_address = Some("sourcegull@example.com".into());
    config.email_smtp = Some(SmtpConfig {
        host: "localhost".into(),
        port: 25,
        username: None,
        password: None,
        authentication: SmtpAuthentication::None,
        no_verify_tls: false,
        encryption: SmtpEncryption::None,
    });
    env.config.store(2, config);
    env.auth
        .sign_up("root", "root@example.com", PW)
        .await
        .unwrap();
    let sender = MemoryEmailSender::default();
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let state = AppState::new(
        env.db.clone(),
        env.config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        env.auth.clone(),
    )
    .with_email_sender(Arc::new(sender.clone()));
    env.router = finish(public_routes().merge(auth_routes()), state);

    let signup = env
        .post(
            "/-/sign-up",
            json!({ "username": "alice", "email": "alice@example.com",
        "password": PW }),
            None,
            true,
        )
        .await;
    assert_eq!(signup.status, StatusCode::OK);
    let cookie = cookie_pair(signup.set_cookie.as_deref().unwrap());
    let verify = sender.messages()[0]
        .text
        .lines()
        .find(|line| line.starts_with("https://"))
        .unwrap()
        .trim_start_matches("https://sourcegull.example")
        .to_string();
    let location = |r: &Resp| r.location.clone().unwrap_or_default();
    let response = env
        .send(Request::get(&verify).body(Body::empty()).unwrap())
        .await;
    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(location(&response), "/user/settings/emails?verified=1");
    let again = env
        .send(Request::get(&verify).body(Body::empty()).unwrap())
        .await;
    assert!(
        location(&again).starts_with("/sign-in?error="),
        "{}",
        location(&again)
    );
    assert!(
        env.db
            .user_emails()
            .list(sourcegull_types::UserId(2))
            .await
            .unwrap()[0]
            .is_verified()
    );

    let init = env
        .post(
            "/-/reset-password-init",
            json!({ "email": "alice@example.com" }),
            None,
            false,
        )
        .await;
    assert_eq!(init.status, StatusCode::OK);
    let unknown = env
        .post(
            "/-/reset-password-init",
            json!({ "email": "missing@example.com" }),
            None,
            false,
        )
        .await;
    assert_eq!(unknown.body, init.body);
    // The reset email is sent in the background.
    for _ in 0..500 {
        if sender.messages().len() > 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let reset = sender.messages()[1]
        .text
        .lines()
        .find(|line| line.starts_with("https://"))
        .unwrap()
        .to_string();
    let url = reqwest::Url::parse(&reset).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let body = json!({ "userID": params["userID"], "code": params["code"],
        "password": "new correct horse battery" });
    let done = env
        .post("/-/reset-password-code", body.clone(), None, false)
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(
        env.post("/-/reset-password-code", body, None, false)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(env.auth_state(Some(&cookie)).await["authenticated"], false);
}

#[tokio::test]
async fn allowlist_trusts_only_configured_proxy_hops() {
    let Some(env) = setup().await else { return };
    let mut config = env.config.load().value.clone();
    config.auth_allowed_ip_address = AllowedIpAddress {
        enabled: true,
        user_ip_address: vec!["192.0.2.0/24".into()],
        trusted_client_ip_address: vec!["10.0.0.0/8".into()],
        client_ip_request_header: "X-Forwarded-For".into(),
    };
    env.config.store(2, config);
    let send = |peer: &str, forwarded: &str| {
        let mut req = Request::get("/-/auth-state")
            .header("X-Forwarded-For", forwarded)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer.parse().unwrap(), 1234)));
        req
    };
    assert_eq!(
        env.send(send("10.0.0.1", "192.0.2.8")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        env.send(send("198.51.100.1", "192.0.2.8")).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        env.send(send("10.0.0.1", "192.0.2.8, 198.51.100.9"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        env.send(send("10.0.0.1", "not-an-ip")).await.status,
        StatusCode::FORBIDDEN
    );
    let mut health = Request::get("/healthz").body(Body::empty()).unwrap();
    health.extensions_mut().insert(ConnectInfo(SocketAddr::new(
        "198.51.100.1".parse().unwrap(),
        1,
    )));
    assert_eq!(env.send(health).await.status, StatusCode::OK);
}

#[tokio::test]
async fn sign_up_sign_in_sign_out_flow() {
    let Some(env) = setup().await else { return };
    let s = env.auth_state(None).await;
    assert_eq!(
        s,
        json!({
            "authenticated": false,
            "allowSignup": false,
            "accessRequest": true,
            "needsSiteInit": true,
            "builtin": true,
            "providers": [],
        })
    );

    // The first sign-up is allowed (site init) and signs the user in.
    let r = env
        .post(
            "/-/sign-up",
            json!({ "username": "root", "email": "root@example.com", "password": PW }),
            None,
            true,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body, json!({ "ok": true }));
    let set = r.set_cookie.expect("Set-Cookie");
    assert!(set.starts_with("sourcegull_session="), "{set}");
    for attr in ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age=2592000"] {
        assert!(set.contains(attr), "{attr} missing in {set}");
    }
    assert!(!set.contains("Secure"), "{set}");
    let cookie = cookie_pair(&set);

    let s = env.auth_state(Some(&cookie)).await;
    assert_eq!(s["authenticated"], true);
    assert_eq!(s["needsSiteInit"], false);
    assert_eq!(
        s["user"],
        json!({ "id": "VXNlcjox", "username": "root", "siteAdmin": true })
    );
    assert_eq!(s["batchChangesEnabled"], true);
    // `batchChanges.restrictToAdmins` leaves bulk changes on for the site admin.
    let mut config = env.config.load().value.clone();
    config.batch_changes_restrict_to_admins = true;
    env.config.store(2, config);
    assert_eq!(
        env.auth_state(Some(&cookie)).await["batchChangesEnabled"],
        true
    );
    let mut config = env.config.load().value.clone();
    config.batch_changes_enabled = false;
    env.config.store(3, config);
    assert_eq!(
        env.auth_state(Some(&cookie)).await["batchChangesEnabled"],
        false
    );

    // Sign-up is closed now (allowSignup: false).
    let r = env
        .post(
            "/-/sign-up",
            json!({ "username": "bob", "email": "bob@example.com", "password": PW }),
            None,
            true,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["code"], "SIGNUP_DISABLED");
    assert!(r.body["error"].is_string());

    // Wrong password.
    let r = env
        .post(
            "/-/sign-in",
            json!({ "username": "root", "password": "nope" }),
            None,
            true,
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.body["code"], "INVALID_CREDENTIALS");
    assert_eq!(
        r.body["error"],
        "Incorrect username or password, or too many failed attempts. Try again later."
    );

    // Sign-in issues a new session; the old one keeps working until signed out.
    let r = env
        .post(
            "/-/sign-in",
            json!({ "username": "root", "password": PW }),
            None,
            true,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let cookie2 = cookie_pair(&r.set_cookie.unwrap());
    assert_ne!(cookie, cookie2);

    // CSRF: cookie-authenticated POST without the header is rejected.
    let r = env
        .post("/-/sign-out", json!({}), Some(&cookie2), false)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["code"], "CSRF");
    assert_eq!(env.auth_state(Some(&cookie2)).await["authenticated"], true);

    // Sign-out with the header ends the session and clears the cookie.
    let r = env
        .post("/-/sign-out", json!({}), Some(&cookie2), true)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let cleared = r.set_cookie.unwrap();
    assert!(
        cleared.starts_with("sourcegull_session=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
    assert_eq!(env.auth_state(Some(&cookie2)).await["authenticated"], false);
    // A stale cookie is anonymous, so POSTs with it need no header.
    let r = env
        .post("/-/sign-out", json!({}), Some(&cookie2), false)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(env.auth_state(Some(&cookie)).await["authenticated"], true);
}

#[tokio::test]
async fn token_auth_is_exempt_from_csrf_and_bad_tokens_fail() {
    let Some(env) = setup().await else { return };
    let user = env
        .auth
        .sign_up("root", "root@example.com", PW)
        .await
        .unwrap();
    let actor = env
        .auth
        .actor_for(user.id, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    let (_, secret) = env
        .auth
        .create_access_token(&actor, user.id, &["user:all".into()], "t", None)
        .await
        .unwrap();

    let req = Request::get("/-/auth-state")
        .header(header::AUTHORIZATION, format!("token {secret}"))
        .body(Body::empty())
        .unwrap();
    let r = env.send(req).await;
    assert_eq!(r.body["authenticated"], true);
    assert_eq!(r.body["user"]["username"], "root");

    let req = Request::post("/-/sign-out")
        .header(header::AUTHORIZATION, format!("token {secret}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(env.send(req).await.status, StatusCode::OK);

    let req = Request::get("/-/auth-state")
        .header(header::AUTHORIZATION, "token skp_bogus")
        .body(Body::empty())
        .unwrap();
    let r = env.send(req).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.body["code"], "TOKEN_INVALID");
}

#[tokio::test]
async fn secure_cookie_and_bad_bodies() {
    let Some(env) = setup().await else { return };
    let mut cfg = env.config.load().value.clone();
    cfg.external_url = Some("https://sourcegull.example.com".into());
    env.config.store(2, cfg);
    let r = env
        .post(
            "/-/sign-up",
            json!({ "username": "root", "email": "root@example.com", "password": PW }),
            None,
            false,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.set_cookie.unwrap().contains("; Secure"));

    let r = env
        .post("/-/sign-in", json!({ "username": "root" }), None, false)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["code"], "INVALID_INPUT");

    let req = Request::post("/-/sign-in")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(
            json!({ "username": "root", "password": PW }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        env.send(req).await.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn trusted_header_auth_and_access_request() {
    let Some(env) = setup().await else { return };
    let cfg = SiteConfig {
        auth_providers: vec![AuthProvider::HttpHeader(HttpHeaderAuthProvider {
            username_header: "x-auth-user".into(),
            email_header: Some("x-auth-email".into()),
            strip_username_header_prefix: Some("accounts.google.com:".into()),
            trusted_proxies: vec!["10.0.0.0/8".into()],
        })],
        ..SiteConfig::default()
    };
    env.config.store(2, cfg);
    let request = |peer: [u8; 4], user: Option<&str>| {
        let mut builder = Request::get("/-/auth-state");
        if let Some(user) = user {
            builder = builder
                .header("x-auth-user", user)
                .header("x-auth-email", "alice@example.com");
        }
        let mut req = builder.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::from(peer)),
            1234,
        )));
        req
    };
    let untrusted = env
        .send(request([192, 0, 2, 1], Some("accounts.google.com:Alice")))
        .await;
    assert_eq!(untrusted.body["authenticated"], false);
    let missing = env.send(request([10, 1, 2, 3], None)).await;
    assert_eq!(missing.body["authenticated"], false);
    let first = env
        .send(request([10, 1, 2, 3], Some("accounts.google.com:Alice")))
        .await;
    assert_eq!(first.body["user"]["username"], "Alice");
    let mut cross_site = Request::post("/-/sign-out")
        .header("x-auth-user", "accounts.google.com:Alice")
        .body(Body::empty())
        .unwrap();
    cross_site
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
            1234,
        )));
    assert_eq!(env.send(cross_site).await.status, StatusCode::FORBIDDEN);
    let again = env
        .send(request([10, 1, 2, 3], Some("accounts.google.com:Alice")))
        .await;
    assert_eq!(again.body["user"]["username"], "Alice");
    assert_eq!(env.db.users().count().await.unwrap(), 1);
    let user = env
        .db
        .users()
        .get_by_username("Alice")
        .await
        .unwrap()
        .unwrap();
    let emails = env.db.user_emails().list(user.id).await.unwrap();
    assert!(emails[0].verified_at.is_some());
    let links = env
        .db
        .external_accounts(None)
        .list_for_user(user.id)
        .await
        .unwrap();
    assert_eq!(links[0].service_type, "http-header");
    let events = env
        .db
        .security_events()
        .list_recent(Some(user.id), 10)
        .await
        .unwrap();
    assert!(events.iter().any(|e| e.name == "SignUpSucceeded"));
    let existing = env
        .db
        .accounts()
        .create(&sourcegull_db::NewAccount {
            user: sourcegull_db::NewUser {
                username: "existing".into(),
                ..Default::default()
            },
            email: Some("other@example.com".into()),
            email_verified: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut req = Request::get("/-/auth-state")
        .header("x-auth-user", "accounts.google.com:Other")
        .header("x-auth-email", "other@example.com")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
        1234,
    )));
    let linked = env.send(req).await;
    assert_eq!(linked.body["user"]["username"], "existing");
    assert_eq!(env.db.users().count().await.unwrap(), 2);
    assert_eq!(
        env.db
            .external_accounts(None)
            .list_for_user(existing.user.id)
            .await
            .unwrap()
            .len(),
        1
    );
    // An unlinked identity matches the user with the same username (no email needed).
    let bob = env
        .db
        .users()
        .create(&sourcegull_db::NewUser {
            username: "bob".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut req = Request::get("/-/auth-state")
        .header("x-auth-user", "accounts.google.com:bob")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
        1234,
    )));
    let by_name = env.send(req).await;
    assert_eq!(by_name.body["user"]["username"], "bob");
    assert_eq!(env.db.users().count().await.unwrap(), 3);
    assert_eq!(
        env.db
            .external_accounts(None)
            .list_for_user(bob.id)
            .await
            .unwrap()[0]
            .service_type,
        "http-header"
    );
    let mut closed = env.config.load().value.clone();
    if let AuthProvider::HttpHeader(provider) = &mut closed.auth_providers[0] {
        provider.trusted_proxies.clear();
    }
    env.config.store(3, closed);
    let denied = env
        .send(request([10, 1, 2, 3], Some("accounts.google.com:Alice")))
        .await;
    assert_eq!(denied.body["authenticated"], false);
    let token = sourcegull_crypto::session::generate_session_id();
    sourcegull_db::sqlx::query(
        "INSERT INTO access_request_invites (token_hash, user_id, expires_at) VALUES ($1, $2, now() + interval '1 hour')",
    )
    .bind(sourcegull_crypto::session::hash_session_id(&token).to_vec())
    .bind(existing.user.id)
    .execute(env.db.pool())
    .await
    .unwrap();
    let set = env
        .post(
            "/.api/set-invite-password",
            json!({"token": token, "password": PW}),
            None,
            false,
        )
        .await;
    assert_eq!(set.status, StatusCode::OK);
    let again = env
        .post(
            "/.api/set-invite-password",
            json!({"token": token, "password": PW}),
            None,
            false,
        )
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert!(
        env.db
            .users()
            .password_hash(existing.user.id)
            .await
            .unwrap()
            .is_some()
    );
    let submitted = env
        .post(
            "/.api/request-access",
            json!({"name":"Bob", "email":"bob@example.com", "additionalInfo":"team"}),
            None,
            false,
        )
        .await;
    assert_eq!(submitted.status, StatusCode::OK);
    let duplicate = env
        .post(
            "/.api/request-access",
            json!({"name":"Bob", "email":"bob@example.com"}),
            None,
            false,
        )
        .await;
    assert_eq!(duplicate.body, submitted.body);
    let existing_email = env
        .post(
            "/.api/request-access",
            json!({"name":"Alice", "email":"alice@example.com"}),
            None,
            false,
        )
        .await;
    assert_eq!(existing_email.body, submitted.body);
}

#[tokio::test]
async fn password_reset_revokes_tokens_and_oauth_grants() {
    let Some(env) = setup().await else { return };
    let user = env
        .auth
        .sign_up("root", "root@example.com", PW)
        .await
        .unwrap();
    let actor = env
        .auth
        .actor_for(user.id, AuthVia::Session)
        .await
        .unwrap()
        .unwrap();
    let (_, secret) = env
        .auth
        .create_access_token(&actor, user.id, &["user:all".into()], "t", None)
        .await
        .unwrap();
    let grant: i64 = sqlx::query_scalar(
        "WITH c AS (INSERT INTO oauth_clients (client_id, kind, redirect_uris)
                    VALUES ('agent', 'registered', ARRAY['http://127.0.0.1/cb']) RETURNING id)
         INSERT INTO oauth_grants (client_id, user_id, resource, scopes)
         SELECT c.id, $1, 'https://sourcegull.example/.api/mcp', ARRAY['mcp'] FROM c RETURNING id",
    )
    .bind(user.id)
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    let with_token = |secret: &str| {
        Request::get("/-/auth-state")
            .header(header::AUTHORIZATION, format!("token {secret}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        env.send(with_token(&secret)).await.body["authenticated"],
        true
    );

    let code = "reset-code";
    env.db
        .password_resets()
        .create(
            user.id,
            &sourcegull_crypto::session::hash_session_id(code),
            3600,
        )
        .await
        .unwrap();
    let done = env
        .post(
            "/-/reset-password-code",
            json!({ "userID": sourcegull_services::relay::marshal_id("User", user.id.get()),
                "code": code, "password": "new correct horse battery" }),
            None,
            false,
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);

    assert_eq!(
        env.send(with_token(&secret)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        env.db.access_tokens().count(Some(user.id)).await.unwrap(),
        0
    );
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM oauth_grants WHERE id = $1")
            .bind(grant)
            .fetch_one(env.db.pool())
            .await
            .unwrap();
    assert!(revoked);
}
