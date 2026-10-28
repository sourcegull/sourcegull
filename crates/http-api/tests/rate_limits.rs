//! `auth.rateLimits` on sign-in, password reset, access requests and rejected webhook
//! deliveries, and `auth.lockout` per account and client address.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull_authn::{Authenticator, PasswordParams, PgSessionStore};
use sourcegull_config::{
    AuthProvider, BuiltinAuthProvider, ConfigHandle, RequestLimit, SiteConfig, SmtpAuthentication,
    SmtpConfig, SmtpEncryption,
};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::{auth_routes, finish, public_routes};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_services::email::MemoryEmailSender;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const PW: &str = "correct horse battery";
const OWNER: &str = "198.51.100.7";
const STRANGER: &str = "203.0.113.9";

struct Env {
    _t: TestDb,
    db: Db,
    router: Router,
    config: ConfigHandle<SiteConfig>,
    sender: MemoryEmailSender,
}

struct Resp {
    status: StatusCode,
    retry_after: Option<u64>,
    body: Value,
}

async fn setup(edit: impl FnOnce(&mut SiteConfig)) -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let mut site = SiteConfig {
        external_url: Some("https://sourcegull.example".into()),
        auth_providers: vec![AuthProvider::Builtin(BuiltinAuthProvider {
            allow_signup: false,
        })],
        email_address: Some("sourcegull@example.com".into()),
        email_smtp: Some(SmtpConfig {
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            authentication: SmtpAuthentication::None,
            no_verify_tls: false,
            encryption: SmtpEncryption::None,
        }),
        ..SiteConfig::default()
    };
    edit(&mut site);
    let config = ConfigHandle::new(1, site);
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        config.clone(),
        PasswordParams::TEST,
    );
    auth.sign_up("root", "root@example.com", PW).await.unwrap();
    let sender = MemoryEmailSender::default();
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let state = AppState::new(
        db.clone(),
        config.clone(),
        MetricsHandle::from_prometheus(recorder.handle()),
        auth,
    )
    .with_email_sender(Arc::new(sender.clone()));
    let router = finish(public_routes().merge(auth_routes()), state);
    Some(Env {
        _t: t,
        db,
        router,
        config,
        sender,
    })
}

impl Env {
    async fn post(&self, path: &str, body: Value, from: &str) -> Resp {
        let mut req = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Requested-With", "Sourcegull")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            from.parse::<IpAddr>().unwrap(),
            1234,
        )));
        let resp = self.router.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().parse().unwrap());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Resp {
            status,
            retry_after,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn sign_in(&self, username: &str, password: &str, from: &str) -> Resp {
        self.post(
            "/-/sign-in",
            json!({ "username": username, "password": password }),
            from,
        )
        .await
    }
}

fn limited(r: &Resp, period: u64) {
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert_eq!(r.body["code"], "RATE_LIMITED");
    let secs = r.retry_after.expect("Retry-After");
    assert!(secs >= 1 && secs <= period, "Retry-After {secs}");
}

#[tokio::test]
async fn sign_in_is_limited_per_address() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.sign_in_per_address = RequestLimit::new(3, 60);
    })
    .await
    else {
        return;
    };
    for _ in 0..3 {
        let r = env.sign_in("root", "wrong password", STRANGER).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Every attempt counts, even with the right password.
    limited(&env.sign_in("root", PW, STRANGER).await, 60);
    // Another address is not affected.
    assert_eq!(env.sign_in("root", PW, OWNER).await.status, StatusCode::OK);
    // Off: no limit.
    let mut c = env.config.load().value.clone();
    c.auth_rate_limits.enabled = false;
    env.config.store(2, c);
    assert_eq!(
        env.sign_in("root", PW, STRANGER).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn failed_sign_ins_are_limited_per_account() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.failed_sign_ins_per_account = RequestLimit::new(3, 600);
        c.auth_lockout.failed_attempt_threshold = 100;
    })
    .await
    else {
        return;
    };
    // Three addresses guess root's password, and an unknown account the same way.
    for (i, login) in ["root", "Root", " root "].iter().enumerate() {
        let from = format!("192.0.2.{}", i + 1);
        let r = env.sign_in(login, "wrong password", &from).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = env.sign_in("ghost", "wrong password", &from).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    // Both accounts are now limited from every address, alike.
    limited(
        &env.sign_in("root", "wrong password", "192.0.2.9").await,
        600,
    );
    limited(
        &env.sign_in("ghost", "wrong password", "192.0.2.9").await,
        600,
    );
    // Other accounts are not.
    assert_eq!(
        env.sign_in("nobody", "wrong password", "192.0.2.9")
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_stranger_cannot_lock_out_the_owner() {
    let Some(env) = setup(|c| {
        c.auth_lockout.failed_attempt_threshold = 5;
        c.auth_lockout.lockout_period = 3600;
    })
    .await
    else {
        return;
    };
    let unknown = env.sign_in("ghost", "wrong password", STRANGER).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    // Five failures lock root, the only site admin, for the stranger's address.
    for _ in 0..5 {
        let r = env.sign_in("root", "wrong password", STRANGER).await;
        assert_eq!((r.status, &r.body), (unknown.status, &unknown.body));
    }
    let id = env
        .db
        .users()
        .get_by_username("root")
        .await
        .unwrap()
        .unwrap()
        .id;
    let from_stranger = env.db.users().login_locked_until(id, STRANGER).await;
    assert!(from_stranger.unwrap().is_some());
    // From there even the right password fails, with the same answer as an unknown account.
    let locked = env.sign_in("root", PW, STRANGER).await;
    assert_eq!(
        (locked.status, &locked.body),
        (unknown.status, &unknown.body)
    );
    // The owner signs in from their own address.
    let owner = env.sign_in("root", PW, OWNER).await;
    assert_eq!(owner.status, StatusCode::OK, "{}", owner.body);
    assert_eq!(
        env.sign_in("root@example.com", PW, OWNER).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn password_reset_is_limited_and_sent_in_the_background() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.password_reset_per_address = RequestLimit::new(4, 60);
        c.auth_rate_limits.password_reset_per_email = RequestLimit::new(2, 60);
    })
    .await
    else {
        return;
    };
    let reset = |email: &'static str, from: &'static str| {
        env.post("/-/reset-password-init", json!({ "email": email }), from)
    };
    let known = reset("root@example.com", OWNER).await;
    assert_eq!(known.status, StatusCode::OK);
    let unknown = reset("missing@example.com", OWNER).await;
    assert_eq!((unknown.status, &unknown.body), (known.status, &known.body));
    // The email is sent after the answer.
    let mut waited = Duration::ZERO;
    while env.sender.messages().is_empty() {
        assert!(waited < Duration::from_secs(10), "no reset email");
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    }
    assert_eq!(env.sender.messages()[0].to, "root@example.com");

    // Per email, from any address, known or not.
    assert_eq!(
        reset("ROOT@example.com", STRANGER).await.status,
        StatusCode::OK
    );
    limited(&reset("root@example.com", STRANGER).await, 60);
    assert_eq!(
        reset("missing@example.com", STRANGER).await.status,
        StatusCode::OK
    );
    limited(&reset("missing@example.com", STRANGER).await, 60);
    // Per address: OWNER made 2 requests, STRANGER 4 (two of them refused).
    assert_eq!(reset("a@example.com", OWNER).await.status, StatusCode::OK);
    assert_eq!(reset("b@example.com", OWNER).await.status, StatusCode::OK);
    limited(&reset("c@example.com", OWNER).await, 60);
    limited(&reset("d@example.com", STRANGER).await, 60);
}

#[tokio::test]
async fn access_requests_are_limited() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.access_request_per_address = RequestLimit::new(3, 60);
        c.auth_rate_limits.access_request_per_email = RequestLimit::new(1, 60);
    })
    .await
    else {
        return;
    };
    let request = |email: &'static str, from: &'static str| {
        env.post(
            "/.api/request-access",
            json!({ "name": "Bob", "email": email }),
            from,
        )
    };
    assert_eq!(
        request("bob@example.com", OWNER).await.status,
        StatusCode::OK
    );
    limited(&request("bob@example.com", STRANGER).await, 60);
    assert_eq!(
        request("carol@example.com", OWNER).await.status,
        StatusCode::OK
    );
    assert_eq!(
        request("dave@example.com", OWNER).await.status,
        StatusCode::OK
    );
    limited(&request("erin@example.com", OWNER).await, 60);
    assert_eq!(
        request("erin@example.com", STRANGER).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn rejected_webhook_deliveries_are_limited_per_address() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.failed_webhooks_per_address = RequestLimit::new(3, 60);
    })
    .await
    else {
        return;
    };
    for _ in 0..3 {
        let r = env.post("/.api/webhooks/nope", json!({}), STRANGER).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    limited(
        &env.post("/.api/webhooks/nope", json!({}), STRANGER).await,
        60,
    );
    limited(
        &env.post("/.api/gitlab-webhooks", json!({}), STRANGER).await,
        60,
    );
    // Deliveries from the code host's own address go on.
    let r = env.post("/.api/webhooks/nope", json!({}), OWNER).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn limits_count_the_address_a_trusted_proxy_reports() {
    let Some(env) = setup(|c| {
        c.auth_rate_limits.sign_in_per_address = RequestLimit::new(2, 60);
        c.auth_allowed_ip_address.trusted_client_ip_address = vec!["10.0.0.0/8".into()];
    })
    .await
    else {
        return;
    };
    let via_proxy = |client: &'static str| {
        let mut req = Request::post("/-/sign-in")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-Forwarded-For", client)
            .body(Body::from(
                json!({ "username": "root", "password": "wrong password" }).to_string(),
            ))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            "10.1.2.3".parse::<IpAddr>().unwrap(),
            1234,
        )));
        let router = env.router.clone();
        async move { router.oneshot(req).await.unwrap().status() }
    };
    assert_eq!(via_proxy(STRANGER).await, StatusCode::UNAUTHORIZED);
    assert_eq!(via_proxy(STRANGER).await, StatusCode::UNAUTHORIZED);
    assert_eq!(via_proxy(STRANGER).await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(via_proxy(OWNER).await, StatusCode::UNAUTHORIZED);
    // An untrusted peer cannot pick its address.
    let r = env.sign_in("root", "wrong password", "192.0.2.50").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}
