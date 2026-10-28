//! API client and discovery tests against the mock GitHub server.

use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use sourcegull_codehost::{
    ClientOptions, CodeHostError, Credential, ListOptions, RepoMeta, RepoSource,
};
use sourcegull_codehost_github::testing::{MockGitHub, repo};
use sourcegull_codehost_github::{
    GitHubClient, GitHubConnection, GitHubSource, validate_connection,
};
use sourcegull_types::Visibility;

fn fast() -> ClientOptions {
    ClientOptions {
        retry_backoff: Duration::from_millis(1),
        ..ClientOptions::default()
    }
}

async fn mock() -> MockGitHub {
    let m = MockGitHub::start("tok").await;
    m.with(|s| {
        s.repos = vec![
            repo(1, "acme/api"),
            repo(2, "acme/web"),
            repo(3, "acme/empty"),
            repo(4, "other/tool"),
            repo(5, "acme/legacy"),
        ];
        s.repos[1]["visibility"] = "internal".into();
        s.repos[1]["fork"] = true.into();
        s.repos[1]["topics"] = serde_json::json!(["svelte"]);
        s.repos[2]["size"] = 0.into();
        s.repos[3]["private"] = false.into();
        s.repos[3]["visibility"] = "public".into();
        s.repos[4]["archived"] = true.into();
        s.affiliated = [4, 1].into();
    });
    m
}

fn source(m: &MockGitHub, extra: &str) -> GitHubSource {
    let text = format!(r#"{{"url": "{}", "token": "tok" {extra}}}"#, m.url);
    let conn: GitHubConnection = validate_connection(&text).unwrap().config;
    let client = Arc::new(GitHubClient::new(&conn, fast()).unwrap());
    GitHubSource::new(conn, client).unwrap()
}

async fn list(s: &GitHubSource) -> Vec<RepoMeta> {
    s.list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap()
}

fn names(repos: &[RepoMeta]) -> Vec<String> {
    repos.iter().map(|r| r.name.clone()).collect()
}

fn host(m: &MockGitHub) -> &str {
    m.url.trim_start_matches("http://")
}

#[tokio::test]
async fn lists_org_repos_with_pagination_and_maps_fields() {
    let m = mock().await;
    let s = source(&m, r#", "orgs": ["acme"]"#);
    let repos = list(&s).await;
    let h = host(&m);
    assert_eq!(
        names(&repos),
        ["api", "web", "empty", "legacy"].map(|n| format!("{h}/acme/{n}"))
    );
    // Pages of two: two requests for four repositories.
    let org_requests = m
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("/api/v3/orgs/acme/repos"))
        .count();
    assert_eq!(org_requests, 2);

    let web = &repos[1];
    assert_eq!(web.external.id, "2");
    assert_eq!(web.external.service_type, "github");
    assert_eq!(web.external.service_id, format!("{}/", m.url));
    assert_eq!(web.visibility, Visibility::Internal);
    assert!(web.private && web.fork && !web.archived);
    assert_eq!(web.topics, ["svelte"]);
    assert_eq!(web.uri, format!("{h}/acme/web"));
    assert_eq!(web.url, "https://github.example.com/acme/web");
    assert_eq!(web.clone_url, "https://github.example.com/acme/web.git");
    assert_eq!(web.default_branch.as_deref(), Some("main"));
    assert_eq!(web.metadata["nameWithOwner"], "acme/web");
    assert!(repos[2].empty);
    assert!(repos[3].archived);
}

#[tokio::test]
async fn combines_repos_orgs_and_queries_without_duplicates() {
    let m = mock().await;
    let s = source(
        &m,
        r#", "repos": ["other/tool", "acme/api"], "repositoryQuery": ["affiliated", "web"],
           "exclude": [{"archived": true}, {"name": "ACME/EMPTY"}],
           "repositoryPathPattern": "gh/{nameWithOwner}",
           "nameTransformations": [{"regex": "^gh/acme/", "replacement": "gh/a/"}]"#,
    );
    let repos = list(&s).await;
    assert_eq!(names(&repos), ["gh/other/tool", "gh/a/api", "gh/a/web"]);
    let tool = &repos[0];
    assert_eq!(tool.visibility, Visibility::Public);
    assert!(!tool.private);
    assert!(
        m.requests()
            .iter()
            .any(|r| r.starts_with("/api/v3/search/repositories?q=web"))
    );
}

#[tokio::test]
async fn lists_public_repositories_by_since() {
    let m = mock().await;
    m.with(|s| {
        for r in &mut s.repos {
            r["private"] = false.into();
            r["visibility"] = "public".into();
        }
    });
    let s = source(&m, r#", "repositoryQuery": ["public"]"#);
    let repos = list(&s).await;
    assert_eq!(repos.len(), 5);
    let requests = m.requests();
    assert!(
        requests.iter().any(|r| r.contains("since=2")),
        "{requests:?}"
    );
    // Each minimal item is completed by id.
    assert!(requests.iter().any(|r| r == "/api/v3/repositories/5"));
}

#[tokio::test]
async fn missing_repo_is_a_partial_failure() {
    let m = mock().await;
    let s = source(&m, r#", "repos": ["acme/gone", "acme/api"]"#);
    let items: Vec<_> =
        futures::StreamExt::collect::<Vec<_>>(s.list_repos(ListOptions::default())).await;
    assert!(matches!(&items[0], Err(CodeHostError::NotFound(m)) if m.contains("acme/gone")));
    assert_eq!(
        items[1].as_ref().unwrap().name,
        format!("{}/acme/api", host(&m))
    );
}

#[tokio::test]
async fn get_repo_by_id_and_credentials() {
    let m = mock().await;
    let s = source(&m, r#", "orgs": ["acme"]"#);
    let r = s.get_repo("5").await.unwrap();
    assert_eq!(r.name, format!("{}/acme/legacy", host(&m)));
    assert!(matches!(
        s.get_repo("../user").await,
        Err(CodeHostError::NotFound(_))
    ));
    assert_eq!(
        s.credential(),
        Credential::Basic {
            username: "x-access-token".into(),
            password: "tok".into()
        }
    );
    let url = s.clone_url(&r, &s.credential()).unwrap();
    assert_eq!(
        url.as_str(),
        "https://x-access-token:tok@github.example.com/acme/legacy.git"
    );
    let ssh = source(&m, r#", "gitURLType": "ssh""#);
    assert_eq!(ssh.credential(), Credential::None);
}

#[tokio::test]
async fn check_reports_account_scopes_and_warnings() {
    let m = mock().await;
    let s = source(&m, r#", "orgs": ["acme"]"#);
    let report = s.check().await.unwrap();
    assert_eq!(report.account.as_deref(), Some("octocat"));
    assert_eq!(report.scopes, Some(vec!["repo".to_owned()]));
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("without an authorization block"));
    let synced = source(
        &m,
        r#", "orgs": ["acme"], "authorization": {"identityProvider": {"type": "oauth"}}"#,
    );
    assert_eq!(synced.check().await.unwrap().warnings, Vec::<String>::new());

    m.with(|s| s.scopes = Some(vec!["read:org".into()]));
    let report = s.check().await.unwrap();
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("lacks the repo scope"));

    let bad = {
        let text = format!(r#"{{"url": "{}", "token": "wrong"}}"#, m.url);
        let conn = validate_connection(&text).unwrap().config;
        GitHubSource::new(
            conn.clone(),
            Arc::new(GitHubClient::new(&conn, fast()).unwrap()),
        )
        .unwrap()
    };
    assert_eq!(bad.check().await.unwrap_err(), CodeHostError::Unauthorized);
}

#[tokio::test]
async fn waits_out_the_primary_rate_limit() {
    let m = mock().await;
    m.with(|s| s.rate_limited = 2);
    let s = source(&m, r#", "repos": ["acme/api"]"#);
    let repos = list(&s).await;
    assert_eq!(repos.len(), 1);
    assert_eq!(m.requests().len(), 3);

    m.with(|s| {
        s.rate_limited = 1;
        s.rate_limit_reset_secs = 3600;
    });
    let err = s.get_repo("1").await.unwrap_err();
    assert!(matches!(err, CodeHostError::RateLimited { .. }), "{err:?}");
}
