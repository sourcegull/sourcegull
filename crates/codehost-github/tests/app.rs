//! GitHub App authentication tests against the mock GitHub server: installation tokens are
//! minted with a signed JWT, cached, refreshed before expiry and minted once under
//! concurrency, and used for discovery, clones and repository permissions.

use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use sourcegull_codehost::{
    AuthzProvider, ClientOptions, CodeHostError, Credential, ListOptions, RepoSource,
};
use sourcegull_codehost_github::testing::{APP_PRIVATE_KEY, MockGitHub, MockUser, repo};
use sourcegull_codehost_github::{
    GitHubAuthzProvider, GitHubClient, GitHubConnection, GitHubSource, validate_connection,
};

const APP_ID: u64 = 4242;
const INSTALLATION_ID: u64 = 77;

fn fast() -> ClientOptions {
    ClientOptions {
        retry_backoff: Duration::from_millis(1),
        ..ClientOptions::default()
    }
}

/// A mock acting as App [`APP_ID`]; the connection token "unused" is never sent.
async fn mock() -> MockGitHub {
    let m = MockGitHub::start("unused").await;
    m.enable_app(APP_ID, INSTALLATION_ID);
    m.with(|s| {
        s.repos = (1..=4).map(|i| repo(i, &format!("acme/r{i}"))).collect();
        s.affiliated = [1, 2, 3].into();
        s.users = vec![
            MockUser::new(7, "alice").reads(1).reads(2),
            MockUser::new(8, "bob").reads(1),
        ];
    });
    m
}

fn conn_with(m: &MockGitHub, app_id: u64, installation_id: u64, extra: &str) -> GitHubConnection {
    let text = format!(
        r#"{{"url": "{}", "githubApp": {{"appID": {app_id}, "privateKey": {}, "installationID": {installation_id}}} {extra}}}"#,
        m.url,
        serde_json::to_string(APP_PRIVATE_KEY).unwrap(),
    );
    validate_connection(&text).unwrap().config
}

fn conn(m: &MockGitHub, extra: &str) -> GitHubConnection {
    conn_with(m, APP_ID, INSTALLATION_ID, extra)
}

fn mints(m: &MockGitHub) -> usize {
    m.requests()
        .iter()
        .filter(|r| r.ends_with("/access_tokens"))
        .count()
}

#[tokio::test]
async fn discovery_and_clones_use_installation_tokens() {
    let m = mock().await;
    let c = conn(
        &m,
        r#", "repositoryQuery": ["affiliated"], "repos": ["acme/r4"]"#,
    );
    let client = Arc::new(GitHubClient::new(&c, fast()).unwrap());
    let s = GitHubSource::new(c, client).unwrap();

    let repos: Vec<_> = s
        .list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap();
    let mut ids: Vec<_> = repos.iter().map(|r| r.external.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["1", "2", "3", "4"]);
    // `affiliated` lists the installation's repositories (two pages); one token served all.
    let reqs = m.requests();
    assert!(
        reqs.iter()
            .any(|r| r.starts_with("/api/v3/installation/repositories")),
        "{reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.starts_with("/api/v3/user")),
        "{reqs:?}"
    );
    assert_eq!(
        reqs[0],
        format!("/api/v3/app/installations/{INSTALLATION_ID}/access_tokens")
    );
    assert_eq!(m.installation_tokens_issued(), 1);

    // Clones authenticate as `x-access-token` with the installation token.
    let cred = s.current_credential().await.unwrap();
    assert_eq!(
        cred,
        Credential::Basic {
            username: "x-access-token".into(),
            password: "ghs_installation_1".into(),
        }
    );
    assert_eq!(s.credential(), cred);
    let url = s.clone_url(&repos[0], &cred).unwrap();
    assert_eq!(url.username(), "x-access-token");
    assert_eq!(url.password(), Some("ghs_installation_1"));

    // The check reports the App installation instead of a user.
    let report = s.check().await.unwrap();
    assert_eq!(
        report.account.as_deref(),
        Some("GitHub App 4242 (installation 77)")
    );
    assert_eq!(mints(&m), 1);

    // SSH clones carry no credential.
    let c = conn(&m, r#", "gitURLType": "ssh""#);
    let client = Arc::new(GitHubClient::new(&c, fast()).unwrap());
    let ssh = GitHubSource::new(c, client).unwrap();
    assert_eq!(ssh.current_credential().await.unwrap(), Credential::None);
}

#[tokio::test]
async fn tokens_are_refreshed_before_expiry_and_after_revocation() {
    let m = mock().await;
    // Tokens that expire within the 5-minute margin are replaced before every use.
    m.with(|s| s.installation_token_ttl_secs = 4 * 60);
    let client = GitHubClient::new(&conn(&m, ""), fast()).unwrap();
    client.repo_by_id("1").await.unwrap();
    client.repo_by_id("2").await.unwrap();
    assert_eq!(m.installation_tokens_issued(), 2);

    // An hour-long token is reused.
    m.with(|s| s.installation_token_ttl_secs = 3600);
    client.repo_by_id("1").await.unwrap();
    client.repo_by_id("2").await.unwrap();
    client.repo_by_id("3").await.unwrap();
    assert_eq!(m.installation_tokens_issued(), 3);

    // GitHub forgets the token (revoked, or expired early): the 401 mints a new one and the
    // request is retried.
    m.with(|s| s.installation_tokens.clear());
    let r = client.repo_by_id("4").await.unwrap();
    assert_eq!(r.id, 4);
    assert_eq!(m.installation_tokens_issued(), 1);
    assert_eq!(
        client.installation_token().await.unwrap().as_deref(),
        Some("ghs_installation_1")
    );
}

#[tokio::test]
async fn concurrent_requests_mint_one_token() {
    let m = mock().await;
    m.with(|s| s.mint_delay = Duration::from_millis(200));
    let c = conn(&m, "");
    let client = Arc::new(GitHubClient::new(&c, fast()).unwrap());
    // A second client of the same connection shares the token.
    let other = Arc::new(GitHubClient::new(&c, fast()).unwrap());
    let tasks: Vec<_> = (0..12)
        .map(|i| {
            let c = if i % 2 == 0 {
                client.clone()
            } else {
                other.clone()
            };
            tokio::spawn(async move { c.repo_by_id(&(i % 4 + 1).to_string()).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(mints(&m), 1);
    assert_eq!(m.installation_tokens_issued(), 1);
}

#[tokio::test]
async fn wrong_app_or_installation_is_a_configuration_error() {
    let m = mock().await;
    let client = GitHubClient::new(&conn_with(&m, 9, INSTALLATION_ID, ""), fast()).unwrap();
    let err = client.repo_by_id("1").await.unwrap_err();
    assert!(
        matches!(err, CodeHostError::Config(ref msg) if msg.contains("appID")),
        "{err}"
    );
    let client = GitHubClient::new(&conn_with(&m, APP_ID, 5, ""), fast()).unwrap();
    let err = client.repo_by_id("1").await.unwrap_err();
    assert!(
        matches!(err, CodeHostError::Config(ref msg) if msg.contains("installationID")),
        "{err}"
    );
    assert_eq!(m.installation_tokens_issued(), 0);
}

#[tokio::test]
async fn repo_centric_permissions_with_app_auth() {
    let m = mock().await;
    let c = conn(
        &m,
        r#", "orgs": ["acme"], "authorization": {"identityProvider": {"type": "oauth"}}"#,
    );
    let client = Arc::new(GitHubClient::new(&c, fast()).unwrap());
    let p = GitHubAuthzProvider::new(&c, client)
        .unwrap()
        .expect("authorization block");
    let users = p.fetch_repo_perms("1").await.unwrap();
    let ids: Vec<_> = users.iter().map(|u| u.username.as_str()).collect();
    assert_eq!(ids, ["alice", "bob"]);
    let users = p.fetch_repo_perms("2").await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(mints(&m), 1);
    assert!(
        m.requests()
            .iter()
            .any(|q| q.starts_with("/api/v3/repos/acme/r1/collaborators?affiliation=all"))
    );

    // An installation without the permission: the error names it.
    m.with(|s| s.collaborators_forbidden = true);
    let err = p.fetch_repo_perms("1").await.unwrap_err();
    assert!(
        matches!(err, CodeHostError::Forbidden(ref msg) if msg.contains("Members (read)")),
        "{err}"
    );
}
