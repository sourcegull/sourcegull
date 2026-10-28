//! Repository permissions tests against the mock GitHub server.

use std::sync::Arc;
use std::time::Duration;

use sourcegull_codehost::{AuthzProvider, ClientOptions, CodeHostError, ExternalAccountData};
use sourcegull_codehost_github::testing::{MockGitHub, MockUser, repo};
use sourcegull_codehost_github::{GitHubAuthzProvider, GitHubClient, validate_connection};

/// Repos: 1 private, 2 private, 3 internal, 4 public, 5 private. Alice (7) reads 1, 3 and 4;
/// bob (8) reads 1, 2 and 5.
async fn mock() -> MockGitHub {
    let m = MockGitHub::start("admin-token").await;
    m.with(|s| {
        s.repos = (1..=5).map(|i| repo(i, &format!("acme/r{i}"))).collect();
        s.repos[2]["visibility"] = "internal".into();
        s.repos[3]["visibility"] = "public".into();
        s.repos[3]["private"] = false.into();
        s.users = vec![
            MockUser::new(7, "alice").reads(1).reads(3).reads(4),
            MockUser::new(8, "bob").reads(1).reads(2).reads(5),
        ];
    });
    m
}

fn provider(m: &MockGitHub, extra: &str) -> GitHubAuthzProvider {
    let text = format!(
        r#"{{"url": "{}", "token": "admin-token", "orgs": ["acme"],
            "authorization": {{"identityProvider": {{"type": "oauth"}}}} {extra}}}"#,
        m.url
    );
    let conn = validate_connection(&text).unwrap().config;
    let client = GitHubClient::new(
        &conn,
        ClientOptions {
            retry_backoff: Duration::from_millis(1),
            ..ClientOptions::default()
        },
    )
    .unwrap();
    GitHubAuthzProvider::new(&conn, Arc::new(client))
        .unwrap()
        .expect("authorization block")
}

fn account(id: &str, token: Option<&str>) -> ExternalAccountData {
    ExternalAccountData {
        account_id: id.into(),
        username: String::new(),
        access_token: token.map(str::to_owned),
        expires_at: None,
    }
}

#[tokio::test]
async fn user_centric_lists_private_and_internal_repos_with_the_users_token() {
    let m = mock().await;
    let p = provider(&m, "");
    assert_eq!(p.service_type(), "github");
    assert_eq!(p.service_id(), format!("{}/", m.url));
    assert!(p.needs_user_token());
    assert!(p.supports_repo_perms());

    let perms = p
        .fetch_user_perms(&account("7", Some("token-7")))
        .await
        .unwrap();
    // The public repo 4 needs no permissions.
    assert_eq!(perms.external_ids, ["1", "3"]);
    let perms = p
        .fetch_user_perms(&account("8", Some("token-8")))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1", "2", "5"]);
    assert!(m.requests().iter().any(|q| q.starts_with(
        "/api/v3/user/repos?affiliation=owner,collaborator,organization_member&per_page=100"
    )));
    // Pages of 2 were followed.
    assert!(m.requests().iter().any(|q| q.contains("page=2")));

    // markInternalReposAsPublic leaves internal repositories out.
    let p = provider(&m, r#", "markInternalReposAsPublic": true"#);
    let perms = p
        .fetch_user_perms(&account("7", Some("token-7")))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1"]);
}

#[tokio::test]
async fn user_centric_token_errors() {
    let m = mock().await;
    let p = provider(&m, "");
    // No token, or a revoked one.
    assert!(matches!(
        p.fetch_user_perms(&account("7", None)).await,
        Err(CodeHostError::Unauthorized)
    ));
    assert!(matches!(
        p.fetch_user_perms(&account("7", Some("revoked"))).await,
        Err(CodeHostError::Unauthorized)
    ));
    // A classic token without `repo` (signed in before permissions were enabled).
    m.with(|s| s.users[0].scopes = Some(vec!["read:user".into(), "user:email".into()]));
    let err = p
        .fetch_user_perms(&account("7", Some("token-7")))
        .await
        .unwrap_err();
    assert!(matches!(err, CodeHostError::MissingScope(ref s) if s == "repo"));
    assert!(err.to_string().contains("sign in again"), "{err}");
    // Tokens that report no scopes (GitHub App user tokens) are used as they are.
    m.with(|s| s.users[0].scopes = None);
    let perms = p
        .fetch_user_perms(&account("7", Some("token-7")))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1", "3"]);
}

#[tokio::test]
async fn repo_centric_lists_collaborators_with_the_connection_token() {
    let m = mock().await;
    let p = provider(&m, "");
    let users = p.fetch_repo_perms("1").await.unwrap();
    let ids: Vec<_> = users
        .iter()
        .map(|u| (u.account_id.as_str(), u.username.as_str()))
        .collect();
    assert_eq!(ids, [("7", "alice"), ("8", "bob")]);
    assert!(users.iter().all(|u| u.access_token.is_none()));
    assert!(
        m.requests()
            .iter()
            .any(|q| q.starts_with("/api/v3/repos/acme/r1/collaborators?affiliation=all"))
    );
    let users = p.fetch_repo_perms("5").await.unwrap();
    assert_eq!(users.len(), 1);

    // Without admin access GitHub refuses; the error says what the token needs.
    m.with(|s| s.collaborators_forbidden = true);
    let err = p.fetch_repo_perms("1").await.unwrap_err();
    assert!(
        matches!(err, CodeHostError::Forbidden(ref msg) if msg.contains("admin access")),
        "{err}"
    );
    // An unknown repository.
    assert!(matches!(
        p.fetch_repo_perms("99").await,
        Err(CodeHostError::NotFound(_))
    ));
}

#[tokio::test]
async fn no_provider_without_authorization() {
    let m = mock().await;
    let text = format!(r#"{{"url": "{}", "token": "t", "orgs": ["acme"]}}"#, m.url);
    let conn = validate_connection(&text).unwrap().config;
    let client = GitHubClient::new(&conn, ClientOptions::default()).unwrap();
    assert!(
        GitHubAuthzProvider::new(&conn, Arc::new(client))
            .unwrap()
            .is_none()
    );
}
