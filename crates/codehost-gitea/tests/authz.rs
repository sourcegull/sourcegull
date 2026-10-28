//! Repository permissions and sign-in token refresh against the mock Gitea server.

use std::sync::Arc;
use std::time::Duration;

use sourcegull_codehost::{
    AuthzProvider, ClientOptions, CodeHostError, ExternalAccountData, OAuthLoginProvider,
};
use sourcegull_codehost_gitea::testing::{MockGitea, MockTeam, MockUser, repo};
use sourcegull_codehost_gitea::{GiteaAuthzProvider, GiteaClient, GiteaOAuth, validate_connection};

/// Repositories: 1 `acme/p1` private, 2 `acme/p2` private, 3 `acme/p3` internal, 4 `acme/p4`
/// public, 5 `alice/own` private (owned by alice). Alice (7): collaborator on 1, on team 10
/// (read) with 3 and 4. Bob (8): collaborator on 1, on team 11 (code unit `none`) with 2.
/// Dave (9) is deactivated, a collaborator on 1. Team 12 (owner) has every repository of acme
/// and member carol (10).
async fn mock() -> MockGitea {
    let m = MockGitea::start("admin-token").await;
    m.with(|s| {
        s.repos = vec![
            repo(1, "acme/p1"),
            repo(2, "acme/p2"),
            repo(3, "acme/p3"),
            repo(4, "acme/p4"),
            repo(5, "alice/own"),
        ];
        s.repos[2]["private"] = false.into();
        s.repos[2]["internal"] = true.into();
        s.repos[3]["private"] = false.into();
        s.users = vec![
            MockUser::new(7, "alice"),
            MockUser::new(8, "bob"),
            MockUser {
                active: false,
                ..MockUser::new(9, "dave")
            },
            MockUser::new(10, "carol"),
        ];
        s.collaborators.insert(1, vec![7, 8, 9]);
        let mut readers = MockTeam::new(10, "acme", "read");
        readers.repos = vec![3, 4];
        readers.members = vec![7];
        let mut no_code = MockTeam::new(11, "acme", "none");
        no_code.code = Some("none".into());
        no_code.repos = vec![2];
        no_code.members = vec![8];
        let mut owners = MockTeam::new(12, "acme", "owner");
        owners.repos = vec![1, 2, 3, 4];
        owners.members = vec![10];
        s.teams = vec![readers, no_code, owners];
    });
    m
}

fn provider(m: &MockGitea, identity: &str) -> GiteaAuthzProvider {
    let text = format!(
        r#"{{"url": "{}", "token": "admin-token", "repositoryQuery": ["all"],
            "authorization": {{"identityProvider": {{"type": "{identity}"}}}}}}"#,
        m.url
    );
    let conn = validate_connection(&text).unwrap().config;
    let client = GiteaClient::new(
        &conn,
        ClientOptions {
            retry_backoff: Duration::from_millis(1),
            ..ClientOptions::default()
        },
    )
    .unwrap();
    GiteaAuthzProvider::new(&conn, Arc::new(client))
        .unwrap()
        .unwrap()
}

fn account(id: &str, username: &str, token: Option<String>) -> ExternalAccountData {
    ExternalAccountData {
        account_id: id.into(),
        username: username.into(),
        access_token: token,
        expires_at: None,
    }
}

fn ids(accounts: &[ExternalAccountData]) -> Vec<&str> {
    accounts.iter().map(|a| a.account_id.as_str()).collect()
}

#[tokio::test]
async fn no_authorization_block_no_provider() {
    let m = mock().await;
    let conn = validate_connection(&format!(
        r#"{{"url": "{}", "token": "t", "orgs": ["acme"]}}"#,
        m.url
    ))
    .unwrap()
    .config;
    let client = Arc::new(GiteaClient::new(&conn, ClientOptions::default()).unwrap());
    assert!(GiteaAuthzProvider::new(&conn, client).unwrap().is_none());
}

#[tokio::test]
async fn oauth_user_perms_with_the_users_token() {
    let m = mock().await;
    let p = provider(&m, "oauth");
    assert_eq!(p.service_type(), "gitea");
    assert_eq!(p.service_id(), format!("{}/", m.url));
    assert!(p.needs_user_token());
    assert!(!p.supports_repo_perms());

    // Alice reads p1 (collaborator), p3 (internal, team) and her own repository; p4 is
    // public and not listed.
    let token = m.issue_token(7);
    let perms = p
        .fetch_user_perms(&account("7", "alice", Some(token)))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1", "3", "5"]);
    // Paginated (the mock serves two items per page).
    assert!(
        m.requests()
            .iter()
            .any(|q| q.starts_with("/api/v1/user/repos") && q.contains("page=2"))
    );

    // Bob's team does not read code on p2.
    let token = m.issue_token(8);
    let perms = p
        .fetch_user_perms(&account("8", "bob", Some(token)))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1"]);

    // No token, or a revoked one.
    assert!(matches!(
        p.fetch_user_perms(&account("7", "alice", None)).await,
        Err(CodeHostError::Unauthorized)
    ));
    assert!(
        p.fetch_user_perms(&account("7", "alice", Some("revoked".into())))
            .await
            .is_err()
    );
    // A token signed in before the scope was asked for.
    let token = m.issue_token(7);
    m.with(|s| s.scopeless_tokens.insert(token.clone()));
    assert!(matches!(
        p.fetch_user_perms(&account("7", "alice", Some(token))).await,
        Err(CodeHostError::MissingScope(ref s)) if s == "read:repository"
    ));
    // OAuth accounts come from signing in, and there is no repo-centric sync.
    assert_eq!(p.fetch_account("alice", &[]).await.unwrap(), None);
    assert!(matches!(
        p.fetch_repo_perms("1").await,
        Err(CodeHostError::Unsupported(_))
    ));
}

#[tokio::test]
async fn username_identity_sudo_and_repo_centric() {
    let m = mock().await;
    let p = provider(&m, "username");
    assert!(!p.needs_user_token());
    assert!(p.supports_repo_perms());

    // Accounts by username: deactivated and unknown users have none.
    let alice = p.fetch_account("Alice", &[]).await.unwrap().unwrap();
    assert_eq!(
        (alice.account_id.as_str(), alice.username.as_str()),
        ("7", "alice")
    );
    assert_eq!(p.fetch_account("dave", &[]).await.unwrap(), None);
    assert_eq!(p.fetch_account("nobody", &[]).await.unwrap(), None);

    // User-centric with the admin token acting as the user.
    let perms = p.fetch_user_perms(&alice).await.unwrap();
    assert_eq!(perms.external_ids, ["1", "3", "5"]);
    let bob = p.fetch_account("bob", &[]).await.unwrap().unwrap();
    assert_eq!(p.fetch_user_perms(&bob).await.unwrap().external_ids, ["1"]);

    // Repo-centric: p1's collaborators (dave is deactivated) and the owners team.
    assert_eq!(
        ids(&p.fetch_repo_perms("1").await.unwrap()),
        ["10", "7", "8"]
    );
    // p2: bob's team does not read code.
    assert_eq!(ids(&p.fetch_repo_perms("2").await.unwrap()), ["10"]);
    // p3: the read team and the owners team.
    assert_eq!(ids(&p.fetch_repo_perms("3").await.unwrap()), ["10", "7"]);
    // A user's repository: its owner (no team lookup).
    assert_eq!(ids(&p.fetch_repo_perms("5").await.unwrap()), ["7"]);
    assert!(
        !m.requests()
            .iter()
            .any(|q| q.starts_with("/api/v1/repos/alice/own/teams"))
    );
    assert!(matches!(
        p.fetch_repo_perms("99").await,
        Err(CodeHostError::NotFound(_))
    ));
}

#[tokio::test]
async fn sign_in_tokens_refresh() {
    let m = mock().await;
    m.with(|s| {
        s.refresh_tokens.insert("rt-1".into(), 7);
    });
    let app = GiteaOAuth::new(
        &m.url,
        "mock-client",
        "mock-secret",
        "http://sourcegull.test",
    )
    .unwrap();
    let token = app.refresh("rt-1").await.unwrap();
    assert_ne!(token.access_token, "admin-token");
    let new_refresh = token.refresh_token.clone().unwrap();
    assert_ne!(new_refresh, "rt-1");
    assert!(token.expires_at.is_some());
    // The new token reads as the user; the old refresh token is spent.
    let perms = provider(&m, "oauth")
        .fetch_user_perms(&account("7", "alice", Some(token.access_token)))
        .await
        .unwrap();
    assert_eq!(perms.external_ids, ["1", "3", "5"]);
    assert!(app.refresh("rt-1").await.is_err());
    assert!(app.refresh(&new_refresh).await.is_ok());
}
