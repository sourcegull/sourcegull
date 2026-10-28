//! OAuth connection tokens: refreshed before expiry and after a 401, rotated tokens stored,
//! a rejected refresh token reported without retrying.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sourcegull_codehost::{CodeHostError, OAuthLoginProvider, RepoSource, Result};
use sourcegull_codehost_gitlab::config::set_oauth_tokens;
use sourcegull_codehost_gitlab::token::REFRESH_REJECTED;
use sourcegull_codehost_gitlab::{
    ClientOptions, GitLabClient, GitLabOAuth, GitLabSource, OAuthTokens, TokenRefresher,
    TokenSession, validate_connection,
};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Refreshes with an OAuth application on the mock server and stores the tokens in a
/// configuration text, as the database-backed refresher does.
#[derive(Debug)]
struct ConfigRefresher {
    app: GitLabOAuth,
    config: Mutex<String>,
}

#[async_trait]
impl TokenRefresher for ConfigRefresher {
    async fn refresh(&self, stale: &OAuthTokens) -> Result<OAuthTokens> {
        let refresh = stale.refresh_token.as_deref().unwrap_or_default();
        let t = self.app.refresh(refresh).await?;
        let new = OAuthTokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: t.expires_at.map(|at| at.timestamp()),
        };
        let mut config = self.config.lock().unwrap();
        *config = set_oauth_tokens(&config, &new).unwrap();
        Ok(new)
    }
}

struct Setup {
    server: MockServer,
    client: GitLabClient,
    refresher: Arc<ConfigRefresher>,
    text: String,
}

async fn setup(expires_in: i64) -> Setup {
    let server = MockServer::start().await;
    let expiry = chrono::Utc::now().timestamp() + expires_in;
    let text = format!(
        r#"{{"url": "{}", "token": "access-1", "token.type": "oauth",
            "token.oauth.refresh": "refresh-1", "token.oauth.expiry": {expiry},
            "projects": [{{"name": "g/p"}}]}}"#,
        server.uri()
    );
    let conn = validate_connection(&text).unwrap().config;
    let refresher = Arc::new(ConfigRefresher {
        app: GitLabOAuth::for_refresh(&server.uri(), "app-id", "app-secret", None).unwrap(),
        config: Mutex::new(text.clone()),
    });
    let session = Arc::new(TokenSession::new(
        OAuthTokens::from_connection(&conn).unwrap(),
        refresher.clone(),
    ));
    let client = GitLabClient::new(&conn, ClientOptions::default())
        .unwrap()
        .with_token_session(session);
    Setup {
        server,
        client,
        refresher,
        text,
    }
}

fn token_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "access-2",
        "refresh_token": "refresh-2",
        "token_type": "Bearer",
        "expires_in": 7200,
        "created_at": chrono::Utc::now().timestamp(),
        "scope": "read_api"
    }))
}

async fn mock_refresh(server: &MockServer, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-1"))
        .and(body_string_contains("client_id=app-id"))
        .and(body_string_contains("client_secret=app-secret"))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

async fn mock_version(server: &MockServer, token: &str, status: u16) {
    Mock::given(method("GET"))
        .and(path("/api/v4/version"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(
            ResponseTemplate::new(status).set_body_json(serde_json::json!({"version": "17.0.0"})),
        )
        .mount(server)
        .await;
}

fn stored(r: &ConfigRefresher) -> sourcegull_codehost_gitlab::GitLabConnection {
    validate_connection(&r.config.lock().unwrap())
        .unwrap()
        .config
}

#[tokio::test]
async fn refreshes_before_expiry_and_stores_rotated_tokens() {
    let s = setup(60).await;
    mock_refresh(&s.server, token_response(), 1).await;
    mock_version(&s.server, "access-2", 200).await;

    // Concurrent requests share one refresh.
    let results =
        futures::future::join_all((0..5).map(|_| s.client.get::<serde_json::Value>("version")))
            .await;
    for r in results {
        r.unwrap();
    }
    let c = stored(&s.refresher);
    assert_eq!(c.token, "access-2");
    assert_eq!(c.token_oauth_refresh.as_deref(), Some("refresh-2"));
    let expiry = c.token_oauth_expiry.unwrap();
    assert!(expiry > chrono::Utc::now().timestamp() + 7000, "{expiry}");
    assert_eq!(c.projects.len(), 1, "other settings are kept");

    // Clones use the refreshed token.
    let source = GitLabSource::new(
        validate_connection(&s.text).unwrap().config,
        Arc::new(s.client.clone()),
    )
    .unwrap();
    match source.current_credential().await.unwrap() {
        sourcegull_codehost::Credential::Basic { username, password } => {
            assert_eq!(username, "oauth2");
            assert_eq!(password, "access-2");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn refreshes_after_unauthorized_and_retries_once() {
    let s = setup(3600).await;
    mock_refresh(&s.server, token_response(), 1).await;
    mock_version(&s.server, "access-1", 401).await;
    mock_version(&s.server, "access-2", 200).await;

    let v: serde_json::Value = s.client.get("version").await.unwrap();
    assert_eq!(v["version"], "17.0.0");
    assert_eq!(stored(&s.refresher).token, "access-2");
    // Later requests use the new token without refreshing again.
    let _: serde_json::Value = s.client.get("version").await.unwrap();
}

#[tokio::test]
async fn a_rejected_refresh_token_fails_without_retrying() {
    let s = setup(3600).await;
    mock_refresh(
        &s.server,
        ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": "invalid_grant"})),
        1,
    )
    .await;
    mock_version(&s.server, "access-1", 401).await;

    for _ in 0..3 {
        match s.client.get::<serde_json::Value>("version").await {
            Err(CodeHostError::Config(m)) => assert_eq!(m, REFRESH_REJECTED),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(stored(&s.refresher).token, "access-1", "nothing stored");

    // New tokens from the configuration (an admin's edit) clear the failure.
    let session = s.client.token_session().unwrap();
    session.observe(&OAuthTokens {
        access_token: "access-3".into(),
        refresh_token: Some("refresh-3".into()),
        expires_at: None,
    });
    mock_version(&s.server, "access-3", 200).await;
    let _: serde_json::Value = s.client.get("version").await.unwrap();
}

#[tokio::test]
async fn stale_configuration_does_not_undo_a_refresh() {
    let s = setup(60).await;
    mock_refresh(&s.server, token_response(), 1).await;
    mock_version(&s.server, "access-2", 200).await;
    let _: serde_json::Value = s.client.get("version").await.unwrap();

    // A client built from the configuration read before the refresh keeps the new token.
    let session = s.client.token_session().unwrap();
    let old = OAuthTokens::from_connection(&validate_connection(&s.text).unwrap().config).unwrap();
    session.observe(&old);
    assert_eq!(session.current().access_token, "access-2");
}
