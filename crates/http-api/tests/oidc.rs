//! OpenID Connect sign-in against a local discovery, token and JWKS server.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Deserialize;
use serde_json::{Value, json};
use sourcegull_authn::{Authenticator, PasswordParams, PgSessionStore};
use sourcegull_config::{AuthProvider, ConfigHandle, OpenIdConnectAuthProvider, SiteConfig};
use sourcegull_crypto::StaticKeyProvider;
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::{auth_routes, finish, public_routes};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const EXTERNAL: &str = "http://sourcegull.test";
const N: &str = "6i13AJWE-WhkNGvv9Ik-vsA5OZprnNN1ONYkb245XO3cyPIh26BeA-A3m5EK0j9hpLfElS7Oy330LKkGzZ7e8MZO70-gl_ORJDYn22MwM4kdu-OIAwTCOb8j5rjsUQDZdBzLQ2PSUnlF-mv8m-ZtezIKRGxm6E1HAAYG_csP0iq3E8wvBieLyPv3YzfXvnswoDbdaUw1EyVPKv5wVJeJncMlVhJ1vXyX_4EPQ5r2pVMwdKhZ03LRDDJq-FIDdPtexvthSsviUavXtTFY6htquo7XxkyKKdNyxtW8d9u7LqHj82CS8MeS7Wfq4ixjRTbC0tbxGOPVBGgCn8uGnWXOFQ";

#[derive(Clone)]
struct IdpState {
    url: String,
    nonce: Arc<Mutex<String>>,
    mode: Arc<Mutex<&'static str>>,
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    state: String,
    nonce: String,
    redirect_uri: String,
}

async fn discovery(State(s): State<IdpState>) -> Json<Value> {
    Json(json!({
        "issuer": s.url,
        "authorization_endpoint": format!("{}/authorize", s.url),
        "token_endpoint": format!("{}/token", s.url),
        "jwks_uri": format!("{}/jwks", s.url),
        "userinfo_endpoint": format!("{}/userinfo", s.url),
    }))
}

async fn authorize(State(s): State<IdpState>, Query(q): Query<AuthorizeQuery>) -> Redirect {
    *s.nonce.lock().unwrap() = q.nonce;
    Redirect::to(&format!("{}?code=ok&state={}", q.redirect_uri, q.state))
}

async fn jwks() -> Json<Value> {
    Json(
        json!({"keys": [{"kty": "RSA", "use": "sig", "alg": "RS256", "kid": "test", "n": N, "e": "AQAB"}]}),
    )
}

async fn userinfo(State(s): State<IdpState>) -> Json<Value> {
    let verified = *s.mode.lock().unwrap() != "userinfo_unverified";
    Json(
        json!({"sub": "subject-7", "email": "alice@example.com", "email_verified": verified,
        "preferred_username": "alice"}),
    )
}

async fn token(State(s): State<IdpState>) -> Json<Value> {
    let mode = *s.mode.lock().unwrap();
    let nonce = s.nonce.lock().unwrap().clone();
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": s.url,
        "sub": "subject-7",
        "aud": if mode == "wrong_aud" { "other-client" } else { "mock-client" },
        "exp": if mode == "expired" { now - 1000 } else { now + 600 },
        "iat": now - 10,
        "nonce": if mode == "bad_nonce" { "other" } else { &nonce },
        "azp": if mode == "wrong_azp" { json!("other-client") } else { Value::Null },
        "email": if mode.starts_with("userinfo") { Value::Null } else { json!("alice@example.com") },
        "email_verified": if mode.starts_with("userinfo") { Value::Null } else { json!(mode != "unverified") },
        "preferred_username": "alice",
        "groups": ["engineering"],
        "roles": ["staff"],
    });
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some("test".into());
    let key = EncodingKey::from_rsa_pem(include_bytes!("fixtures/oidc-test-key.pem")).unwrap();
    Json(json!({"id_token": encode(&h, &claims, &key).unwrap(), "access_token": "access"}))
}

struct Env {
    _db: TestDb,
    db: Db,
    router: Router,
    idp: IdpState,
    config: ConfigHandle<SiteConfig>,
}

fn site_config(issuer: &str, allow_signup: bool, domain: Option<&str>) -> SiteConfig {
    SiteConfig {
        external_url: Some(EXTERNAL.into()),
        auth_providers: vec![AuthProvider::OpenIdConnect(OpenIdConnectAuthProvider {
            config_id: Some("work".into()),
            issuer: issuer.into(),
            client_id: "mock-client".into(),
            client_secret: "mock-secret".into(),
            require_email_domain: domain.map(str::to_owned),
            allow_signup,
            link_by_email: None,
            allow_groups: Vec::new(),
            groups_claim: None,
            display_name: Some("Work SSO".into()),
        })],
        ..SiteConfig::default()
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.unwrap();
    let db = Db::from_pool(t.pool.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let idp = IdpState {
        url: url.clone(),
        nonce: Arc::new(Mutex::new(String::new())),
        mode: Arc::new(Mutex::new("ok")),
    };
    let idp_router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/authorize", get(authorize))
        .route("/jwks", get(jwks))
        .route("/userinfo", get(userinfo))
        .route("/token", post(token))
        .with_state(idp.clone());
    tokio::spawn(async move { axum::serve(listener, idp_router).await.unwrap() });
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let config = ConfigHandle::new(1, site_config(&url, true, None));
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().unwrap();
    let metrics = MetricsHandle::from_prometheus(recorder.handle());
    let state =
        AppState::new(db.clone(), config.clone(), metrics, auth).with_keyring(Some(keyring));
    let router = finish(public_routes().merge(auth_routes()), state);
    Some(Env {
        _db: t,
        db,
        router,
        idp,
        config,
    })
}

struct Resp {
    status: StatusCode,
    location: String,
    cookies: Vec<String>,
    body: Value,
}

impl Env {
    async fn get(&self, path: &str, cookie: Option<&str>) -> Resp {
        let mut request = Request::get(path);
        if let Some(c) = cookie {
            request = request.header(header::COOKIE, c);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp {
            status,
            location,
            cookies,
            body,
        }
    }

    async fn begin(&self) -> (String, String) {
        let login = self
            .get(
                "/.auth/openidconnect/login?pc=work&returnTo=%2Fsearch",
                None,
            )
            .await;
        assert_eq!(login.status, StatusCode::FOUND);
        let authorization = reqwest::Url::parse(&login.location).unwrap();
        let params: std::collections::HashMap<_, _> =
            authorization.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(params.get("nonce").is_some_and(|n| !n.is_empty()));
        let cookie = login.cookies[0].split(';').next().unwrap().to_owned();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let auth = client.get(&login.location).send().await.unwrap();
        assert_eq!(auth.status(), reqwest::StatusCode::SEE_OTHER);
        let path = auth.headers()["location"]
            .to_str()
            .unwrap()
            .strip_prefix(EXTERNAL)
            .unwrap()
            .to_owned();
        (path, cookie)
    }

    async fn sign_in(&self) -> Resp {
        let (path, cookie) = self.begin().await;
        self.get(&path, Some(&cookie)).await
    }
}

fn error(resp: &Resp) -> String {
    let encoded = resp
        .location
        .strip_prefix("/sign-in?error=")
        .unwrap_or(&resp.location);
    percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn success_and_single_use_state() {
    let Some(env) = setup().await else { return };
    let auth = env.get("/-/auth-state", None).await;
    assert_eq!(
        auth.body["providers"][0]["authenticationURL"],
        "/.auth/openidconnect/login?pc=work"
    );
    let (path, cookie) = env.begin().await;
    assert!(
        error(&env.get(&path, Some("sourcegull_oauth_state=wrong")).await)
            .contains("another browser")
    );
    let ok = env.get(&path, Some(&cookie)).await;
    assert_eq!(ok.location, "/search");
    assert!(
        ok.cookies
            .iter()
            .any(|c| c.starts_with("sourcegull_session=") && !c.starts_with("sourcegull_session=;"))
    );
    assert!(error(&env.get(&path, Some(&cookie)).await).contains("expired"));
    assert_eq!(env.db.users().count().await.unwrap(), 1);
    let user = env
        .db
        .users()
        .get_by_username("alice")
        .await
        .unwrap()
        .unwrap();
    let accounts = env
        .db
        .external_accounts(None)
        .list_for_user(user.id)
        .await
        .unwrap();
    assert_eq!(accounts[0].service_type, "openidconnect");
    assert_eq!(accounts[0].service_id, env.idp.url);
    assert_eq!(accounts[0].account_id, "subject-7");
    *env.idp.mode.lock().unwrap() = "userinfo";
    assert_eq!(env.sign_in().await.location, "/search");
    assert_eq!(env.db.users().count().await.unwrap(), 1);
}

#[tokio::test]
async fn verified_email_links_existing_user() {
    let Some(env) = setup().await else { return };
    let created = env
        .db
        .accounts()
        .create(&sourcegull_db::NewAccount {
            user: sourcegull_db::NewUser {
                username: "local-alice".into(),
                ..Default::default()
            },
            email: Some("alice@example.com".into()),
            email_verified: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(env.sign_in().await.location, "/search");
    assert_eq!(env.db.users().count().await.unwrap(), 1);
    let accounts = env
        .db
        .external_accounts(None)
        .list_for_user(created.user.id)
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account_id, "subject-7");
}

#[tokio::test]
async fn rejects_bad_claims_and_policy() {
    let Some(env) = setup().await else { return };
    for mode in [
        "bad_nonce",
        "wrong_aud",
        "wrong_azp",
        "expired",
        "unverified",
        "userinfo_unverified",
    ] {
        *env.idp.mode.lock().unwrap() = mode;
        let response = env.sign_in().await;
        assert!(
            response.location.starts_with("/sign-in?error="),
            "{mode}: {}",
            response.location
        );
        assert_eq!(env.db.users().count().await.unwrap(), 0);
    }
    *env.idp.mode.lock().unwrap() = "ok";
    env.config
        .store(2, site_config(&env.idp.url, true, Some("other.example")));
    assert!(error(&env.sign_in().await).contains("domain"));
    env.db
        .accounts()
        .create(&sourcegull_db::NewAccount {
            user: sourcegull_db::NewUser {
                username: "admin".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    env.config.store(3, site_config(&env.idp.url, false, None));
    assert!(error(&env.sign_in().await).contains("Sign-up is disabled"));
    let mut config = site_config(&env.idp.url, true, None);
    if let AuthProvider::OpenIdConnect(p) = &mut config.auth_providers[0] {
        p.allow_groups = vec!["other".into()];
    }
    env.config.store(4, config);
    assert!(error(&env.sign_in().await).contains("group"));
    let mut config = site_config(&env.idp.url, true, None);
    if let AuthProvider::OpenIdConnect(p) = &mut config.auth_providers[0] {
        p.allow_groups = vec!["engineering".into()];
    }
    env.config.store(5, config);
    assert_eq!(env.sign_in().await.location, "/search");
    let mut config = site_config(&env.idp.url, true, None);
    if let AuthProvider::OpenIdConnect(p) = &mut config.auth_providers[0] {
        p.allow_groups = vec!["staff".into()];
        p.groups_claim = Some("roles".into());
    }
    env.config.store(6, config);
    assert_eq!(env.sign_in().await.location, "/search");
}
