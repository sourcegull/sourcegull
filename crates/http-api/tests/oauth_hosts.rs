//! GitHub and Gitea OAuth sign-in end to end against the mock code hosts: the sign-in buttons,
//! login redirect, the authorize step, the callback, account creation with the verified email,
//! linking, `allowOrgs`, a denied authorization, and a flow started for another host.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sourcegull_authn::{Authenticator, PasswordParams, PgSessionStore};
use sourcegull_codehost::OAuthToken;
use sourcegull_codehost_gitea::testing::MockGitea;
use sourcegull_codehost_github::testing::MockGitHub;
use sourcegull_config::{
    AuthProvider, BuiltinAuthProvider, ConfigHandle, GiteaAuthProvider, GithubAuthProvider,
    SiteConfig,
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
    github: MockGitHub,
    gitea: MockGitea,
    router: Router,
    config: ConfigHandle<SiteConfig>,
}

fn site_config(github_url: &str, gitea_url: &str, allow_orgs: Vec<String>) -> SiteConfig {
    SiteConfig {
        external_url: Some(EXTERNAL.into()),
        auth_providers: vec![
            AuthProvider::Builtin(BuiltinAuthProvider::default()),
            AuthProvider::Github(GithubAuthProvider {
                url: github_url.into(),
                client_id: "mock-client".into(),
                client_secret: "mock-secret".into(),
                allow_signup: true,
                link_by_email: None,
                allow_orgs: allow_orgs.clone(),
                display_name: None,
            }),
            AuthProvider::Gitea(GiteaAuthProvider {
                url: gitea_url.into(),
                client_id: "mock-client".into(),
                client_secret: "mock-secret".into(),
                allow_signup: true,
                link_by_email: None,
                allow_orgs,
                display_name: Some("Example Gitea".into()),
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
    let github = MockGitHub::start("gh-user-token").await;
    github.with(|s| {
        s.user_id = 42;
        s.login = "octocat".into();
        s.name = Some("The Octocat".into());
        s.email = Some("public@example.com".into());
        s.emails = vec![
            json!({"email": "other@example.com", "primary": false, "verified": true}),
            json!({"email": "octo@example.com", "primary": true, "verified": true}),
        ];
        s.orgs = vec!["a".into(), "b".into(), "Acme".into()];
    });
    let gitea = MockGitea::start("gitea-user-token").await;
    gitea.with(|s| {
        s.user_id = 9;
        s.login = "tea".into();
        s.name = Some("Tea Drinker".into());
        s.email = Some("tea@example.com".into());
        s.emails = vec![json!({"email": "tea@example.com", "primary": true, "verified": false})];
        s.orgs = vec!["x".into(), "y".into(), "acme".into()];
    });
    let config = ConfigHandle::new(1, site_config(&github.url, &gitea.url, Vec::new()));
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
        github,
        gitea,
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

    /// Starts the flow at `/.auth/<host>/login` and follows the mock's authorize redirect.
    /// Returns the callback path and the state cookie.
    async fn authorize(&self, host: &str, mock_url: &str) -> (String, String) {
        let login = self
            .get(
                &format!("/.auth/{host}/login?returnTo=%2Fsearch%3Fq%3Dx"),
                None,
            )
            .await;
        assert_eq!(login.status, StatusCode::FOUND);
        let authorize = login.location.unwrap();
        assert!(
            authorize.starts_with(&format!("{mock_url}/login/oauth/authorize?")),
            "{authorize}"
        );
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
            .expect("redirects to externalURL")
            .to_owned();
        assert!(
            path.starts_with(&format!("/.auth/{host}/callback?")),
            "{path}"
        );
        (path, state_cookie)
    }

    async fn sign_in(&self, host: &str, mock_url: &str) -> Resp {
        let (path, cookie) = self.authorize(host, mock_url).await;
        self.get(&path, Some(&cookie)).await
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
async fn github_and_gitea_sign_in() {
    let Some(env) = setup().await else { return };

    let anon = env.get("/-/auth-state", None).await.body;
    assert_eq!(anon["providers"][0]["type"], "github");
    assert_eq!(anon["providers"][0]["displayName"], "GitHub");
    assert_eq!(
        anon["providers"][0]["authenticationURL"],
        "/.auth/github/login?pc=mock%2Dclient"
    );
    assert_eq!(anon["providers"][1]["type"], "gitea");
    assert_eq!(anon["providers"][1]["displayName"], "Example Gitea");
    assert_eq!(
        anon["providers"][1]["authenticationURL"],
        "/.auth/gitea/login?pc=mock%2Dclient"
    );

    // GitHub: the first user becomes site admin, with the primary verified email.
    let r = env.sign_in("github", &env.github.url).await;
    assert_eq!(r.location.as_deref(), Some("/search?q=x"));
    let session = session_cookie(&r);
    let s = env.auth_state(&session).await;
    assert_eq!(s["user"]["username"], "octocat");
    assert_eq!(s["user"]["siteAdmin"], true);
    let octocat = env
        .db
        .users()
        .get_by_username("octocat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(octocat.display_name.as_deref(), Some("The Octocat"));
    let emails = env.db.user_emails().list(octocat.id).await.unwrap();
    assert_eq!(emails[0].email, "octo@example.com");
    assert!(emails[0].verified_at.is_some());
    let accounts = env.db.external_accounts(Some(env.keyring.clone()));
    let acct = accounts
        .get_for_user(octocat.id, "github", &format!("{}/", env.github.url))
        .await
        .unwrap()
        .expect("linked GitHub account");
    assert_eq!(acct.account_id, "42");
    assert_eq!(acct.client_id, "mock-client");
    let token: OAuthToken = serde_json::from_slice(acct.auth_data.as_ref().unwrap()).unwrap();
    assert_eq!(token.access_token, "gh-user-token");
    assert!(token.expires_at.is_none());
    // Without allowOrgs, organizations are neither requested nor listed.
    assert!(
        env.github
            .requests()
            .iter()
            .all(|r| !r.starts_with("/api/v3/user/orgs"))
    );
    let authorize = env
        .github
        .requests()
        .into_iter()
        .find(|r| r.starts_with("/login/oauth/authorize"))
        .unwrap();
    assert!(
        authorize.contains("scope=read%3Auser+user%3Aemail&"),
        "{authorize}"
    );

    // Signing in again reuses the account.
    let r = env.sign_in("github", &env.github.url).await;
    session_cookie(&r);
    assert_eq!(env.db.users().count().await.unwrap(), 1);

    // Gitea: the email is not verified, so it does not match anyone; a new user is created.
    let r = env.sign_in("gitea", &env.gitea.url).await;
    let session = session_cookie(&r);
    let s = env.auth_state(&session).await;
    assert_eq!(s["user"]["username"], "tea");
    assert_eq!(s["user"]["siteAdmin"], false);
    let tea = env
        .db
        .users()
        .get_by_username("tea")
        .await
        .unwrap()
        .unwrap();
    let emails = env.db.user_emails().list(tea.id).await.unwrap();
    assert_eq!(emails[0].email, "tea@example.com");
    assert!(emails[0].verified_at.is_none());
    let acct = accounts
        .get_for_user(tea.id, "gitea", &format!("{}/", env.gitea.url))
        .await
        .unwrap()
        .expect("linked Gitea account");
    assert_eq!(acct.account_id, "9");
    let token: OAuthToken = serde_json::from_slice(acct.auth_data.as_ref().unwrap()).unwrap();
    assert_eq!(token.access_token, "gitea-user-token");
    assert_eq!(token.refresh_token.as_deref(), Some("mock-refresh"));
    assert!(token.expires_at.is_some());

    // A Gitea connection for the same instance syncs permissions: sign-in also asks for
    // `read:repository`.
    let gitea_scope = |env: &Env| {
        env.gitea
            .requests()
            .into_iter()
            .rfind(|r| r.starts_with("/login/oauth/authorize"))
            .unwrap()
    };
    assert!(
        gitea_scope(&env).contains("scope=read%3Auser+read%3Aorganization&"),
        "{}",
        gitea_scope(&env)
    );
    env.db
        .external_services(Some(env.keyring.clone()))
        .create(&sourcegull_db::NewExternalService {
            kind: sourcegull_types::ExternalServiceKind::Gitea,
            display_name: "Gitea".into(),
            config: format!(
                r#"{{"url": "{}", "token": "t", "orgs": ["acme"],
                    "authorization": {{"identityProvider": {{"type": "oauth"}}}}}}"#,
                env.gitea.url
            ),
        })
        .await
        .unwrap();
    env.sign_in("gitea", &env.gitea.url).await;
    assert!(
        gitea_scope(&env).contains("scope=read%3Auser+read%3Aorganization+read%3Arepository&"),
        "{}",
        gitea_scope(&env)
    );
}

#[tokio::test]
async fn allow_orgs_denials_and_state_checks() {
    let Some(env) = setup().await else { return };

    // allowOrgs: matched case-insensitively across pages; read:org is requested on GitHub.
    env.config.store(
        2,
        site_config(&env.github.url, &env.gitea.url, vec!["acme".into()]),
    );
    let r = env.sign_in("github", &env.github.url).await;
    session_cookie(&r);
    assert!(
        env.github
            .requests()
            .iter()
            .any(|r| r.starts_with("/login/oauth/authorize") && r.contains("read%3Aorg"))
    );
    let r = env.sign_in("gitea", &env.gitea.url).await;
    session_cookie(&r);

    env.config.store(
        3,
        site_config(&env.github.url, &env.gitea.url, vec!["other".into()]),
    );
    let r = env.sign_in("github", &env.github.url).await;
    assert_eq!(
        error_of(&r),
        "GitHub user octocat is not a member of an organization allowed to sign in."
    );
    let r = env.sign_in("gitea", &env.gitea.url).await;
    assert_eq!(
        error_of(&r),
        "Gitea user tea is not a member of an organization allowed to sign in."
    );

    // The user declines on the code host.
    env.config
        .store(4, site_config(&env.github.url, &env.gitea.url, Vec::new()));
    env.gitea.with(|s| s.authorize = false);
    let r = env.sign_in("gitea", &env.gitea.url).await;
    assert_eq!(
        error_of(&r),
        "Gitea sign-in was not completed: access_denied"
    );

    // A flow started for GitHub is not taken by the Gitea callback.
    let (path, cookie) = env.authorize("github", &env.github.url).await;
    let other = path.replacen("/.auth/github/", "/.auth/gitea/", 1);
    let r = env.get(&other, Some(&cookie)).await;
    assert_eq!(error_of(&r), "The sign-in attempt expired. Try again.");

    // A bad code is refused even though GitHub answers it with 200.
    let (path, cookie) = env.authorize("github", &env.github.url).await;
    let (before, after) = path.split_once("code=").unwrap();
    let rest = after.split_once('&').map_or("", |(_, r)| r);
    let r = env
        .get(&format!("{before}code=forged&{rest}"), Some(&cookie))
        .await;
    assert_eq!(error_of(&r), "GitHub sign-in failed. Try again.");

    // Unknown provider.
    let r = env.get("/.auth/github/login?pc=nope", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["error"], "Unknown GitHub auth provider.");
}

#[tokio::test]
async fn github_asks_for_repo_when_a_connection_syncs_permissions() {
    let Some(env) = setup().await else { return };
    let authorize_scope = |env: &Env| {
        env.github
            .requests()
            .into_iter()
            .rfind(|r| r.starts_with("/login/oauth/authorize"))
            .and_then(|r| {
                url::Url::parse(&format!("http://x{r}"))
                    .unwrap()
                    .query_pairs()
                    .find(|(k, _)| k == "scope")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap()
    };
    let connection = |authorization: &str| {
        format!(
            r#"{{"url": "{}", "token": "t", "orgs": ["acme"] {authorization}}}"#,
            env.github.url
        )
    };
    let services = env.db.external_services(Some(env.keyring.clone()));

    // A connection without an authorization block: the usual scopes.
    services
        .create(&sourcegull_db::NewExternalService {
            kind: sourcegull_types::ExternalServiceKind::GitHub,
            display_name: "GitHub".into(),
            config: connection(""),
        })
        .await
        .unwrap();
    session_cookie(&env.sign_in("github", &env.github.url).await);
    assert_eq!(authorize_scope(&env), "read:user user:email");

    // One that syncs permissions with the users' tokens: `repo` too.
    services
        .create(&sourcegull_db::NewExternalService {
            kind: sourcegull_types::ExternalServiceKind::GitHub,
            display_name: "GitHub private".into(),
            config: connection(r#", "authorization": {"identityProvider": {"type": "oauth"}}"#),
        })
        .await
        .unwrap();
    session_cookie(&env.sign_in("github", &env.github.url).await);
    assert_eq!(authorize_scope(&env), "read:user user:email repo");
}
