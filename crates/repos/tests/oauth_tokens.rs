//! GitLab OAuth connection tokens refreshed and stored in the connection's configuration,
//! against the test Postgres (`SOURCEGULL_TEST_DATABASE_URL`) and a mock GitLab.

use std::sync::Arc;

use sourcegull_codehost::CodeHostError;
use sourcegull_codehost_gitlab::validate_connection;
use sourcegull_config::{AuthProvider, ConfigHandle, GitlabAuthProvider, SiteConfig};
use sourcegull_crypto::{Keyring, StaticKeyProvider};
use sourcegull_db::{Db, ExternalServiceUpdate, NewExternalService, migrate};
use sourcegull_repos::Registry;
use sourcegull_repos::oauth_tokens::ConnectionTokens;
use sourcegull_testutil::TestDb;
use sourcegull_types::{ExternalServiceId, ExternalServiceKind};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Env {
    _t: TestDb,
    db: Db,
    keyring: Arc<Keyring>,
    server: MockServer,
    id: ExternalServiceId,
    config: sourcegull_config::SiteConfigHandle,
}

async fn setup(expires_in: i64) -> Option<Env> {
    let tdb = TestDb::new().await?;
    migrate::up(&tdb.pool).await.expect("migrate up");
    let db = Db::from_pool(tdb.pool.clone());
    let provider = Arc::new(StaticKeyProvider::new("k1", [3u8; 32]).unwrap());
    let keyring = Arc::new(db.encryption_keys().load_keyring(provider).await.unwrap());
    let server = MockServer::start().await;
    let expiry = chrono::Utc::now().timestamp() + expires_in;
    let config = format!(
        "{{\n  // OAuth connection\n  \"url\": \"{}\", \"token\": \"access-1\", \"token.type\": \"oauth\",\n  \"token.oauth.refresh\": \"refresh-1\", \"token.oauth.expiry\": {expiry},\n  \"projects\": [{{\"name\": \"g/p\"}}]\n}}",
        server.uri()
    );
    let id = db
        .external_services(Some(keyring.clone()))
        .create(&NewExternalService {
            kind: ExternalServiceKind::GitLab,
            display_name: "GitLab".into(),
            config,
        })
        .await
        .unwrap()
        .id;
    let site = SiteConfig {
        auth_providers: vec![
            AuthProvider::Gitlab(GitlabAuthProvider {
                url: "https://other.example.com".into(),
                client_id: "other".into(),
                client_secret: "other".into(),
                allow_signup: false,
                link_by_email: None,
                allow_groups: vec![],
                display_name: None,
            }),
            AuthProvider::Gitlab(GitlabAuthProvider {
                url: server.uri(),
                client_id: "app-id".into(),
                client_secret: "app-secret".into(),
                allow_signup: false,
                link_by_email: None,
                allow_groups: vec![],
                display_name: None,
            }),
        ],
        ..SiteConfig::default()
    };
    Some(Env {
        _t: tdb,
        db,
        keyring,
        server,
        id,
        config: ConfigHandle::new(1, site),
    })
}

impl Env {
    /// A registry as one process builds it, with its own token sessions.
    fn registry(&self) -> Registry {
        Registry {
            oauth_tokens: Some(Arc::new(ConnectionTokens::new(
                self.db.external_services(Some(self.keyring.clone())),
                self.config.clone(),
            ))),
            ..Registry::default()
        }
        .for_connection(self.id.get())
    }

    async fn stored(&self) -> String {
        self.db
            .external_services(Some(self.keyring.clone()))
            .config(self.id)
            .await
            .unwrap()
    }

    async fn check(&self, registry: &Registry, text: &str) -> Result<(), CodeHostError> {
        registry
            .source(ExternalServiceKind::GitLab, text)?
            .check()
            .await
            .map(|_| ())
    }
}

/// Answers `GET /version` and `GET /user` (what a connection check reads) with `status` for
/// requests carrying `token`.
async fn mock_api(server: &MockServer, token: &str, status: u16) {
    for (api, body) in [
        ("/api/v4/version", serde_json::json!({"version": "17.0.0"})),
        ("/api/v4/user", serde_json::json!({"username": "svc"})),
    ] {
        Mock::given(method("GET"))
            .and(path(api))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }
}

async fn mock_refresh(server: &MockServer, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-1"))
        .and(body_string_contains("client_id=app-id"))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

fn rotated() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "access-2",
        "refresh_token": "refresh-2",
        "expires_in": 7200,
        "created_at": chrono::Utc::now().timestamp(),
        "scope": "read_api"
    }))
}

#[tokio::test]
async fn refreshed_tokens_are_stored_and_shared_across_processes() {
    let Some(env) = setup(60).await else { return };
    mock_refresh(&env.server, rotated(), 1).await;
    mock_api(&env.server, "access-1", 401).await;
    mock_api(&env.server, "access-2", 200).await;
    let before = env.stored().await;
    let svc_before = env
        .db
        .external_services(Some(env.keyring.clone()))
        .get(env.id)
        .await
        .unwrap()
        .unwrap();

    // Process A refreshes ahead of expiry and stores the rotated tokens.
    let a = env.registry();
    env.check(&a, &before).await.unwrap();
    let after = env.stored().await;
    assert!(after.contains("// OAuth connection"), "{after}");
    let c = validate_connection(&after).unwrap().config;
    assert_eq!(c.token, "access-2");
    assert_eq!(c.token_oauth_refresh.as_deref(), Some("refresh-2"));
    assert!(c.token_oauth_expiry.unwrap() > chrono::Utc::now().timestamp() + 7000);
    let svc_after = env
        .db
        .external_services(Some(env.keyring.clone()))
        .get(env.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(svc_after.sync_watermark, svc_before.sync_watermark);

    // Process B still holds the configuration read before the refresh: it picks up the stored
    // tokens instead of spending the old refresh token (the mock allows one refresh).
    let b = env.registry();
    env.check(&b, &before).await.unwrap();

    // A's later clients built from the stale text keep the refreshed token.
    env.check(&a, &before).await.unwrap();
}

#[tokio::test]
async fn a_rejected_refresh_token_is_reported_until_the_admin_sets_new_tokens() {
    let Some(env) = setup(3600).await else { return };
    mock_refresh(
        &env.server,
        ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid_grant"})),
        1,
    )
    .await;
    mock_api(&env.server, "access-1", 401).await;
    let text = env.stored().await;
    let r = env.registry();
    for _ in 0..2 {
        let err = env.check(&r, &text).await.unwrap_err();
        assert!(err.to_string().contains("refresh token"), "{err}");
    }
    assert_eq!(env.stored().await, text, "nothing stored");

    // The admin pastes new tokens.
    let new = text
        .replace("access-1", "access-9")
        .replace("refresh-1", "refresh-9");
    env.db
        .external_services(Some(env.keyring.clone()))
        .update(
            env.id,
            &ExternalServiceUpdate {
                config: Some(new.clone()),
                ..ExternalServiceUpdate::default()
            },
        )
        .await
        .unwrap();
    mock_api(&env.server, "access-9", 200).await;
    env.check(&r, &new).await.unwrap();
}
