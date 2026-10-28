//! GitLab OAuth sign-in end to end against the mock GitLab: login redirect, the
//! authorize step, the callback, account creation and linking, tokens stored encrypted,
//! `allowGroups`, and the state checks.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::Value;
use sourcegull_authn::{Authenticator, PasswordParams, PgSessionStore};
use sourcegull_codehost::OAuthToken;
use sourcegull_codehost_gitlab::testing::{MockGitLab, MockUser};
use sourcegull_config::{
    AuthProvider, BuiltinAuthProvider, ConfigHandle, GitlabAuthProvider, SiteConfig,
};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, migrate};
use sourcegull_http_api::{auth_routes, finish, public_routes};
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use sourcegull_testutil::TestDb;
use tower::ServiceExt;

const EXTERNAL: &str = "http://sourcegull.test";

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    gitlab: MockGitLab,
    router: Router,
    config: ConfigHandle<SiteConfig>,
}

fn site_config(gitlab_url: &str, allow_signup: bool, allow_groups: Vec<String>) -> SiteConfig {
    SiteConfig {
        external_url: Some(EXTERNAL.into()),
        auth_providers: vec![
            AuthProvider::Builtin(BuiltinAuthProvider::default()),
            AuthProvider::Gitlab(GitlabAuthProvider {
                url: gitlab_url.into(),
                client_id: "mock-client".into(),
                client_secret: "mock-secret".into(),
                allow_signup,
                link_by_email: None,
                allow_groups,
                display_name: Some("Example GitLab".into()),
            }),
        ],
        ..SiteConfig::default()
    }
}

async fn setup() -> Option<Env> {
    let t = TestDb::new().await?;
    migrate::up(&t.pool).await.expect("migrate");
    let db = Db::from_pool(t.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [5u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let gitlab = MockGitLab::start("admin-token").await;
    gitlab.with(|s| {
        let mut alice = MockUser::new(7, "alice");
        alice.groups = vec!["acme".into(), "acme/web".into()];
        s.users = vec![alice, MockUser::new(8, "bob")];
    });
    let config = ConfigHandle::new(1, site_config(&gitlab.url, true, Vec::new()));
    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        config.clone(),
        PasswordParams::TEST,
    );
    let recorder = sourcegull_observability::build_recorder().expect("recorder");
    let metrics = MetricsHandle::from_prometheus(recorder.handle());
    let state = AppState::new(db.clone(), config.clone(), metrics, auth)
        .with_keyring(Some(keyring.clone()));
    let router = finish(public_routes().merge(auth_routes()), state);
    Some(Env {
        _t: t,
        db,
        keyring,
        gitlab,
        router,
        config,
    })
}

struct Resp {
    status: StatusCode,
    location: Option<String>,
    cookies: Vec<String>,
    body: Value,
}

impl Env {
    async fn get(&self, path: &str, cookie: Option<&str>) -> Resp {
        let mut req = Request::get(path);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let resp = self
            .router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let location = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let cookies = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Resp {
            status,
            location,
            cookies,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// Runs the flow as GitLab user `as_user`: login → authorize → callback. Returns the
    /// callback response.
    async fn sign_in_as(&self, as_user: Option<i64>) -> Resp {
        self.gitlab.with(|s| s.authorize_as = as_user);
        let login = self
            .get("/.auth/gitlab/login?returnTo=%2Fsearch%3Fq%3Dx", None)
            .await;
        assert_eq!(login.status, StatusCode::FOUND);
        let authorize = login.location.unwrap();
        assert!(authorize.starts_with(&self.gitlab.url), "{authorize}");
        let state_cookie = cookie_pair(&login.cookies[0]);
        assert!(state_cookie.starts_with("sourcegull_oauth_state="));

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let back = client.get(&authorize).send().await.unwrap();
        assert_eq!(back.status().as_u16(), 302);
        let callback = back.headers()["location"].to_str().unwrap().to_owned();
        let path = callback
            .strip_prefix(EXTERNAL)
            .expect("redirects to externalURL");
        assert!(path.starts_with("/.auth/gitlab/callback?"), "{path}");
        self.get(path, Some(&state_cookie)).await
    }

    async fn auth_state(&self, session: &str) -> Value {
        self.get("/-/auth-state", Some(session)).await.body
    }
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

fn session_cookie(r: &Resp) -> String {
    r.cookies
        .iter()
        .find(|c| c.starts_with("sourcegull_session=") && !c.starts_with("sourcegull_session=;"))
        .map(|c| cookie_pair(c))
        .expect("a session cookie")
}

fn error_of(r: &Resp) -> String {
    assert_eq!(r.status, StatusCode::FOUND);
    let loc = r.location.clone().unwrap();
    let q = loc
        .strip_prefix("/sign-in?error=")
        .unwrap_or_else(|| panic!("{loc}"));
    percent_encoding::percent_decode_str(q)
        .decode_utf8()
        .unwrap()
        .into_owned()
}

#[tokio::test]
async fn sign_in_creates_links_and_stores_tokens() {
    let Some(env) = setup().await else { return };

    let anon = env.get("/-/auth-state", None).await.body;
    assert_eq!(anon["builtin"], true);
    assert_eq!(anon["providers"][0]["type"], "gitlab");
    assert_eq!(anon["providers"][0]["displayName"], "Example GitLab");
    assert_eq!(
        anon["providers"][0]["authenticationURL"],
        "/.auth/gitlab/login?pc=mock%2Dclient"
    );

    // First sign-in creates alice (the first user: site admin) and signs her in.
    let r = env.sign_in_as(Some(7)).await;
    assert_eq!(r.location.as_deref(), Some("/search?q=x"));
    assert!(
        r.cookies
            .iter()
            .any(|c| c.starts_with("sourcegull_oauth_state=;"))
    );
    let session = session_cookie(&r);
    let s = env.auth_state(&session).await;
    assert_eq!(s["authenticated"], true);
    assert_eq!(s["user"]["username"], "alice");
    assert_eq!(s["user"]["siteAdmin"], true);

    let user = env
        .db
        .users()
        .get_by_username("alice")
        .await
        .unwrap()
        .unwrap();
    let emails = env.db.user_emails().list(user.id).await.unwrap();
    assert_eq!(emails[0].email, "alice@example.com");
    assert!(emails[0].verified_at.is_some());
    let accounts = env.db.external_accounts(Some(env.keyring.clone()));
    let acct = accounts
        .get_for_user(user.id, "gitlab", &format!("{}/", env.gitlab.url))
        .await
        .unwrap()
        .expect("linked account");
    assert_eq!(acct.account_id, "7");
    assert_eq!(acct.client_id, "mock-client");
    assert_eq!(acct.account_data["username"], "alice");
    let token: OAuthToken = serde_json::from_slice(acct.auth_data.as_ref().unwrap()).unwrap();
    assert!(token.refresh_token.is_some());
    assert!(token.expires_at.is_some());
    // The token is stored encrypted.
    let raw: Vec<u8> = sourcegull_db::sqlx::query_scalar(
        "SELECT auth_data_enc FROM user_external_accounts WHERE id = $1",
    )
    .bind(acct.id)
    .fetch_one(env.db.pool())
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(&token.access_token));

    // Signing in again reuses the account and replaces the tokens.
    let r = env.sign_in_as(Some(7)).await;
    session_cookie(&r);
    let again = accounts
        .get_for_user(user.id, "gitlab", &format!("{}/", env.gitlab.url))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, acct.id);
    assert_ne!(again.auth_data, acct.auth_data);
    assert_eq!(env.db.users().count().await.unwrap(), 1);

    // A second user is not an admin; a taken username gets a suffix.
    env.db
        .accounts()
        .create(&sourcegull_db::NewAccount {
            user: sourcegull_db::NewUser {
                username: "bob".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let r = env.sign_in_as(Some(8)).await;
    let s = env.auth_state(&session_cookie(&r)).await;
    assert_eq!(s["user"]["username"], "bob-2");
    assert_eq!(s["user"]["siteAdmin"], false);
}

/// `site_config` with the GitLab provider's `linkByEmail` set to `link`.
fn link_by_email_config(gitlab_url: &str, link: Option<bool>) -> SiteConfig {
    let mut config = site_config(gitlab_url, true, Vec::new());
    for p in &mut config.auth_providers {
        if let AuthProvider::Gitlab(g) = p {
            g.link_by_email = link;
        }
    }
    config
}

#[tokio::test]
async fn verified_email_links_existing_user() {
    let Some(env) = setup().await else { return };
    env.gitlab.with(|s| s.users.push(MockUser::new(9, "carol")));
    let create = |username: &str, email: &str| {
        let new = sourcegull_db::NewAccount {
            user: sourcegull_db::NewUser {
                username: username.into(),
                ..Default::default()
            },
            email: Some(email.into()),
            email_verified: true,
            ..Default::default()
        };
        let db = env.db.clone();
        async move { db.accounts().create(&new).await.unwrap().user }
    };
    // The first user (site admin) has the verified email of GitLab user bob.
    let root = create("root", "bob@example.com").await;
    assert!(root.site_admin);
    let alice = create("alice-local", "alice@example.com").await;
    create("carol-local", "carol@example.com").await;
    let accounts = env.db.external_accounts(Some(env.keyring.clone()));

    // Unset: users are linked by email, site admins included.
    let r = env.sign_in_as(Some(7)).await;
    let s = env.auth_state(&session_cookie(&r)).await;
    assert_eq!(s["user"]["username"], "alice-local");
    assert_eq!(accounts.list_for_user(alice.id).await.unwrap().len(), 1);
    let r = env.sign_in_as(Some(8)).await;
    let s = env.auth_state(&session_cookie(&r)).await;
    assert_eq!(s["user"]["username"], "root");
    assert_eq!(accounts.list_for_user(root.id).await.unwrap().len(), 1);

    // false: nothing is linked by email, and the taken email fails the sign-up clearly.
    env.config
        .store(2, link_by_email_config(&env.gitlab.url, Some(false)));
    let err = error_of(&env.sign_in_as(Some(9)).await);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(env.db.users().count().await.unwrap(), 3);
}

#[tokio::test]
async fn signup_disabled_groups_and_state_checks() {
    let Some(env) = setup().await else { return };
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

    // allowGroups: bob is in no group.
    env.config.store(
        2,
        site_config(&env.gitlab.url, true, vec!["acme/web".into()]),
    );
    let err = error_of(&env.sign_in_as(Some(8)).await);
    assert!(err.contains("not a member of a group"), "{err}");
    // alice is in acme/web.
    session_cookie(&env.sign_in_as(Some(7)).await);

    // Sign-up disabled: bob has no account.
    env.config
        .store(3, site_config(&env.gitlab.url, false, Vec::new()));
    let err = error_of(&env.sign_in_as(Some(8)).await);
    assert!(err.contains("Sign-up is disabled"), "{err}");

    // The user declined on GitLab.
    let err = error_of(&env.sign_in_as(None).await);
    assert!(err.contains("access_denied"), "{err}");

    // The callback needs the state cookie of the browser that started the flow.
    env.gitlab.with(|s| s.authorize_as = Some(7));
    let login = env.get("/.auth/gitlab/login", None).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let back = client.get(login.location.unwrap()).send().await.unwrap();
    let callback = back.headers()["location"].to_str().unwrap().to_owned();
    let path = callback.strip_prefix(EXTERNAL).unwrap().to_owned();
    let err = error_of(&env.get(&path, Some("sourcegull_oauth_state=other")).await);
    assert!(
        err.contains("expired or came from another browser"),
        "{err}"
    );
    let err = error_of(&env.get(&path, None).await);
    assert!(err.contains("another browser"), "{err}");
    // A flow is used once.
    let state_cookie = cookie_pair(&login.cookies[0]);
    session_cookie(&env.get(&path, Some(&state_cookie)).await);
    let err = error_of(&env.get(&path, Some(&state_cookie)).await);
    assert!(err.contains("expired"), "{err}");

    // Unknown provider.
    let r = env.get("/.auth/gitlab/login?pc=nope", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
