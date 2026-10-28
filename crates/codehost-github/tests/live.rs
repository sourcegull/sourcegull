//! Tests against github.com, to catch differences between the mock and GitHub. Run with a
//! personal access token (no scope needed; public repositories only):
//! `SOURCEGULL_TEST_GITHUB_TOKEN=<token> scripts/dev cargo test -p sourcegull-codehost-github --test
//! live`. Without `SOURCEGULL_TEST_GITHUB_TOKEN` every test passes without doing anything; CI does
//! not set it.

use std::sync::Arc;

use futures::TryStreamExt;
use sourcegull_codehost::{ClientOptions, ListOptions, RepoMeta, RepoSource};
use sourcegull_codehost_github::{GitHubClient, GitHubSource, validate_connection};
use sourcegull_types::Visibility;

fn source(extra: &str) -> Option<GitHubSource> {
    let token = std::env::var("SOURCEGULL_TEST_GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())?;
    let text = format!(r#"{{"url": "https://github.com", "token": "{token}" {extra}}}"#);
    let conn = validate_connection(&text).unwrap().config;
    let client = Arc::new(GitHubClient::new(&conn, ClientOptions::default()).unwrap());
    Some(GitHubSource::new(conn, client).unwrap())
}

#[tokio::test]
async fn lists_a_public_repository_and_checks_the_token() {
    let Some(s) = source(r#", "repos": ["octocat/Hello-World"]"#) else {
        return;
    };
    let repos: Vec<RepoMeta> = s
        .list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(repos.len(), 1);
    let r = &repos[0];
    assert_eq!(r.name, "github.com/octocat/Hello-World");
    assert_eq!(r.visibility, Visibility::Public);
    assert!(r.clone_url.ends_with("/octocat/Hello-World.git"));
    // The external id is stable and fetchable.
    let again = s.get_repo(&r.external.id).await.unwrap();
    assert_eq!(again.name, r.name);

    let report = s.check().await.unwrap();
    assert!(report.account.is_some());
}

#[tokio::test]
async fn paginates_an_organization_and_a_search() {
    let Some(s) =
        source(r#", "orgs": ["octokit"], "repositoryQuery": ["org:github topic:hacktoberfest"]"#)
    else {
        return;
    };
    let repos: Vec<RepoMeta> = s
        .list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap();
    // Both the organization listing and the search contribute.
    assert!(
        repos
            .iter()
            .any(|r| r.name.starts_with("github.com/octokit/"))
    );
    assert!(
        repos
            .iter()
            .any(|r| r.name.starts_with("github.com/github/"))
    );
}
