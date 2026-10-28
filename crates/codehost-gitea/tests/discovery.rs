//! API client and discovery tests against the mock Gitea server.

use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use sourcegull_codehost::{
    ClientOptions, CodeHostError, Credential, ListOptions, RepoMeta, RepoSource,
};
use sourcegull_codehost_gitea::testing::{MockGitea, repo};
use sourcegull_codehost_gitea::{GiteaClient, GiteaConnection, GiteaSource, validate_connection};
use sourcegull_types::Visibility;

fn fast() -> ClientOptions {
    ClientOptions {
        retry_backoff: Duration::from_millis(1),
        ..ClientOptions::default()
    }
}

async fn mock() -> MockGitea {
    let m = MockGitea::start("tok").await;
    m.with(|s| {
        s.repos = vec![
            repo(1, "acme/api"),
            repo(2, "acme/web"),
            repo(3, "acme/empty"),
            repo(4, "other/tool"),
            repo(5, "acme/legacy"),
        ];
        s.repos[1]["internal"] = true.into();
        s.repos[1]["private"] = false.into();
        s.repos[1]["fork"] = true.into();
        s.repos[1]["topics"] = serde_json::json!(["svelte"]);
        s.repos[2]["empty"] = true.into();
        s.repos[2]["topics"] = serde_json::Value::Null;
        s.repos[3]["private"] = false.into();
        s.repos[4]["archived"] = true.into();
        s.repos[4]["mirror"] = true.into();
        s.affiliated = [4, 1].into();
    });
    m
}

fn source(m: &MockGitea, extra: &str) -> GiteaSource {
    let text = format!(r#"{{"url": "{}", "token": "tok" {extra}}}"#, m.url);
    let conn: GiteaConnection = validate_connection(&text).unwrap().config;
    let client = Arc::new(GiteaClient::new(&conn, fast()).unwrap());
    GiteaSource::new(conn, client).unwrap()
}

async fn list(s: &GiteaSource) -> Vec<RepoMeta> {
    s.list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap()
}

fn names(repos: &[RepoMeta]) -> Vec<String> {
    repos.iter().map(|r| r.name.clone()).collect()
}

fn host(m: &MockGitea) -> &str {
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
    // Pages of two, smaller than the limit asked for: two requests for four repositories,
    // ended by X-Total-Count.
    let org_requests: Vec<String> = m
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("/api/v1/orgs/acme/repos"))
        .collect();
    assert_eq!(
        org_requests,
        [
            "/api/v1/orgs/acme/repos?page=1&limit=50",
            "/api/v1/orgs/acme/repos?page=2&limit=50"
        ]
    );

    let web = &repos[1];
    assert_eq!(web.external.id, "2");
    assert_eq!(web.external.service_type, "gitea");
    assert_eq!(web.external.service_id, format!("{}/", m.url));
    assert_eq!(web.visibility, Visibility::Internal);
    assert!(web.private && web.fork && !web.archived);
    assert_eq!(web.topics, ["svelte"]);
    assert_eq!(web.uri, format!("{h}/acme/web"));
    assert_eq!(web.url, "https://gitea.example.com/acme/web");
    assert_eq!(web.clone_url, "https://gitea.example.com/acme/web.git");
    assert_eq!(web.default_branch.as_deref(), Some("main"));
    assert_eq!(web.metadata["nameWithOwner"], "acme/web");
    assert!(repos[2].empty);
    assert!(repos[2].topics.is_empty());
    assert!(repos[3].archived);
    assert_eq!(repos[0].visibility, Visibility::Private);
}

#[tokio::test]
async fn paginates_until_an_empty_page_without_a_total() {
    let m = mock().await;
    m.with(|s| s.total_count = false);
    let s = source(&m, r#", "orgs": ["acme"]"#);
    assert_eq!(list(&s).await.len(), 4);
    let pages = m
        .requests()
        .iter()
        .filter(|r| r.starts_with("/api/v1/orgs/acme/repos"))
        .count();
    assert_eq!(pages, 3);
}

#[tokio::test]
async fn combines_repos_orgs_and_queries_without_duplicates() {
    let m = mock().await;
    let s = source(
        &m,
        r#", "repos": ["other/tool", "acme/api"], "repositoryQuery": ["affiliated", "web"],
           "exclude": [{"mirrors": true}, {"name": "ACME/EMPTY"}],
           "repositoryPathPattern": "gt/{nameWithOwner}",
           "nameTransformations": [{"regex": "^gt/acme/", "replacement": "gt/a/"}]"#,
    );
    let repos = list(&s).await;
    assert_eq!(names(&repos), ["gt/other/tool", "gt/a/api", "gt/a/web"]);
    let tool = &repos[0];
    assert_eq!(tool.visibility, Visibility::Public);
    assert!(!tool.private);
    assert!(
        m.requests()
            .iter()
            .any(|r| r.starts_with("/api/v1/repos/search?q=web&page=1"))
    );
}

#[tokio::test]
async fn all_lists_every_readable_repository() {
    let m = mock().await;
    let s = source(&m, r#", "repositoryQuery": ["all"]"#);
    assert_eq!(list(&s).await.len(), 5);
    assert!(
        m.requests()
            .iter()
            .any(|r| r == "/api/v1/repos/search?page=1&limit=50")
    );
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
            username: "sourcegull".into(),
            password: "tok".into()
        }
    );
    let url = s.clone_url(&r, &s.credential()).unwrap();
    assert_eq!(
        url.as_str(),
        "https://sourcegull:tok@gitea.example.com/acme/legacy.git"
    );
    let ssh = source(&m, r#", "gitURLType": "ssh""#);
    assert_eq!(ssh.credential(), Credential::None);
}

#[tokio::test]
async fn check_reports_account_and_warnings() {
    let m = mock().await;
    let s = source(&m, r#", "orgs": ["acme"]"#);
    let report = s.check().await.unwrap();
    assert_eq!(report.account.as_deref(), Some("alice"));
    assert_eq!(report.scopes, None);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("permissions are not synced"));

    m.with(|s| s.version = "1.19.4".into());
    let report = s.check().await.unwrap();
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("older than 1.20"));

    let bad = {
        let text = format!(r#"{{"url": "{}", "token": "wrong"}}"#, m.url);
        let conn = validate_connection(&text).unwrap().config;
        GiteaSource::new(
            conn.clone(),
            Arc::new(GiteaClient::new(&conn, fast()).unwrap()),
        )
        .unwrap()
    };
    assert_eq!(bad.check().await.unwrap_err(), CodeHostError::Unauthorized);
}

#[tokio::test]
async fn waits_out_a_rate_limit() {
    let m = mock().await;
    m.with(|s| s.rate_limited = 2);
    let s = source(&m, r#", "repos": ["acme/api"]"#);
    let repos = list(&s).await;
    assert_eq!(repos.len(), 1);
    assert_eq!(m.requests().len(), 3);

    m.with(|s| {
        s.rate_limited = 1;
        s.retry_after_secs = 3600;
    });
    let err = s.get_repo("1").await.unwrap_err();
    assert!(matches!(err, CodeHostError::RateLimited { .. }), "{err:?}");
}
