//! API client and discovery tests against the mock GitLab server.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::{StreamExt, TryStreamExt};
use sourcegull_codehost::{
    CodeHostError, Credential, ListOptions, RepoMeta, RepoSource, TokenBucket,
};
use sourcegull_codehost_gitlab::testing::{MockGitLab, project};
use sourcegull_codehost_gitlab::{
    Auth, ClientOptions, GitLabClient, GitLabConnection, GitLabSource, validate_connection,
};
use sourcegull_types::Visibility;

fn t(day: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap()
}

fn fast() -> ClientOptions {
    ClientOptions {
        retry_backoff: Duration::from_millis(1),
        ..ClientOptions::default()
    }
}

async fn mock() -> MockGitLab {
    let m = MockGitLab::start("tok").await;
    m.with(|s| {
        s.projects = vec![
            project(1, "acme/api", t(1)),
            project(2, "acme/web", t(2)),
            project(3, "acme/empty", t(3)),
            project(4, "other/tool", t(4)),
            project(5, "acme/legacy", t(5)),
        ];
        s.projects[2]["empty_repo"] = true.into();
        s.projects[1]["visibility"] = "internal".into();
        s.projects[1]["forked_from_project"] =
            serde_json::json!({"id": 99, "path_with_namespace": "up/web"});
        s.projects[1]["topics"] = serde_json::json!(["svelte"]);
        s.projects[0]["repository_access_level"] = "enabled".into();
        s.projects[0]["permissions"] = serde_json::json!({
            "project_access": {"access_level": 10, "notification_level": 3},
            "group_access": {"access_level": 30, "notification_level": 3},
        });
        s.projects[3]["repository_access_level"] = "disabled".into();
        s.projects[3]["permissions"] =
            serde_json::json!({"project_access": null, "group_access": {"access_level": 10}});
        s.groups.insert("acme".into(), vec![1, 2, 3, 5]);
    });
    m
}

fn source(m: &MockGitLab, extra: &str) -> GitLabSource {
    let text = format!(r#"{{"url": "{}", "token": "tok" {extra}}}"#, m.url);
    let conn: GitLabConnection = validate_connection(&text).unwrap().config;
    let client = Arc::new(GitLabClient::new(&conn, fast()).unwrap());
    GitLabSource::new(conn, client).unwrap()
}

async fn list(s: &GitLabSource, opts: ListOptions) -> Vec<RepoMeta> {
    s.list_repos(opts).try_collect().await.unwrap()
}

fn names(repos: &[RepoMeta]) -> Vec<&str> {
    repos.iter().map(|r| r.name.as_str()).collect()
}

#[tokio::test]
async fn lists_projects_with_keyset_pagination_and_maps_fields() {
    let m = mock().await;
    let s = source(&m, r#", "projectQuery": ["projects?membership=true"]"#);
    let repos = list(&s, ListOptions::default()).await;
    let host = m.url.trim_start_matches("http://");
    assert_eq!(
        names(&repos),
        [
            format!("{host}/acme/api"),
            format!("{host}/acme/web"),
            format!("{host}/acme/empty"),
            format!("{host}/other/tool"),
            format!("{host}/acme/legacy"),
        ]
    );
    let reqs = m.requests();
    // version probe + 3 keyset pages of 2.
    let pages: Vec<_> = reqs
        .iter()
        .filter(|r| r.starts_with("/api/v4/projects?"))
        .collect();
    assert_eq!(pages.len(), 3, "{reqs:?}");
    assert!(pages[0].contains("pagination=keyset") && pages[0].contains("membership=true"));
    assert!(pages[1].contains("id_after=2"));

    let web = &repos[1];
    assert_eq!(web.external.id, "2");
    assert_eq!(web.external.service_type, "gitlab");
    assert_eq!(web.external.service_id, format!("{}/", m.url));
    assert_eq!(web.uri, format!("{host}/acme/web"));
    assert!(web.fork && web.private);
    assert_eq!(web.visibility, Visibility::Internal);
    assert_eq!(web.topics, ["svelte"]);
    assert_eq!(web.stars, 2);
    assert_eq!(web.last_activity_at, Some(t(2)));
    assert_eq!(web.clone_url, "https://gitlab.example.com/acme/web.git");
    assert_eq!(web.metadata["forkedFromProject"]["id"], 99);
    // `empty_repo` (no git repository yet) is kept for the repo node and the UI.
    assert_eq!(web.metadata["emptyRepo"], false);
    assert!(!web.empty);
    assert_eq!(repos[2].metadata["emptyRepo"], true);
    assert!(repos[2].empty);
    // The repository feature's access level and the token's effective access (the higher
    // of project and group membership) tell a disabled repository from missing access.
    assert_eq!(repos[0].metadata["repositoryAccessLevel"], "enabled");
    assert_eq!(repos[0].metadata["accessLevel"], 30);
    assert_eq!(repos[3].metadata["repositoryAccessLevel"], "disabled");
    assert_eq!(repos[3].metadata["accessLevel"], 10);
    assert!(web.metadata["repositoryAccessLevel"].is_null());
    assert!(web.metadata["accessLevel"].is_null());

    let cred = s.credential();
    assert!(matches!(&cred, Credential::Basic { username, .. } if username == "git"));
    assert!(!format!("{cred:?}").contains("tok"));
    assert_eq!(
        s.clone_url(web, &cred).unwrap().as_str(),
        "https://git:tok@gitlab.example.com/acme/web.git"
    );
}

#[tokio::test]
async fn explicit_projects_groups_exclusions_and_naming() {
    let m = mock().await;
    let s = source(
        &m,
        r#", "projects": [{"id": 4}, {"name": "acme/api"}],
            "projectQuery": ["groups/acme/projects"],
            "exclude": [{"emptyRepos": true}, {"pattern": "legacy$"}, {"name": "ACME/WEB"}],
            "repositoryPathPattern": "gl/{pathWithNamespace}",
            "nameTransformations": [{"regex": "^gl/acme/", "replacement": "acme-"}],
            "markInternalReposAsPublic": true"#,
    );
    let repos = list(&s, ListOptions::default()).await;
    // Explicit projects first; acme/api is deduplicated; empty, legacy and web excluded.
    assert_eq!(names(&repos), ["gl/other/tool", "acme-api"]);
    let group_pages: Vec<_> = m
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("/api/v4/groups/acme/projects"))
        .collect();
    // Offset pagination (no keyset on group listings): 4 projects in pages of 2.
    assert_eq!(group_pages.len(), 2, "{group_pages:?}");
    assert!(group_pages[1].contains("page=2"));

    let got = s.get_repo("2").await.unwrap();
    assert_eq!(got.name, "acme-web");
    assert!(!got.private, "internal marked public");
    assert!(matches!(
        s.get_repo("404").await,
        Err(CodeHostError::NotFound(_))
    ));
}

#[tokio::test]
async fn incremental_listing_passes_last_activity_after() {
    let m = mock().await;
    let s = source(
        &m,
        r#", "projects": [{"id": 1}], "projectQuery": ["projects?owned=true"]"#,
    );
    assert!(s.supports_incremental());
    let repos = list(
        &s,
        ListOptions {
            since: Some(t(3) + chrono::Duration::hours(1)),
        },
    )
    .await;
    // Only projects active after the watermark (minus the 5 minute overlap), and explicit
    // projects are skipped.
    assert_eq!(
        repos
            .iter()
            .map(|r| r.external.id.as_str())
            .collect::<Vec<_>>(),
        ["4", "5"]
    );
    let reqs = m.requests();
    assert!(
        reqs.iter()
            .any(|r| r.contains("last_activity_after=2026-01-03T00%3A55%3A00Z")),
        "{reqs:?}"
    );
    assert!(!reqs.iter().any(|r| r.starts_with("/api/v4/projects/1")));
}

#[tokio::test]
async fn partial_failures_are_reported_in_the_stream() {
    let m = mock().await;
    let s = source(
        &m,
        r#", "projects": [{"name": "missing/project"}], "projectQuery": ["groups/nope/projects", "groups/acme/projects"]"#,
    );
    let items: Vec<_> = s
        .list_repos(ListOptions::default())
        .collect::<Vec<_>>()
        .await;
    let errors: Vec<_> = items.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(matches!(errors[0], CodeHostError::NotFound(m) if m.contains("missing/project")));
    assert_eq!(items.iter().filter(|r| r.is_ok()).count(), 4);
}

#[tokio::test]
async fn retries_429_and_rejects_bad_tokens() {
    let m = mock().await;
    m.with(|s| s.rate_limited = 2);
    let s = source(&m, r#", "projectQuery": ["projects?membership=true"]"#);
    assert_eq!(list(&s, ListOptions::default()).await.len(), 5);

    // Out of retries.
    m.with(|s| s.rate_limited = 10);
    let client = GitLabClient::with_parts(
        &m.url,
        Auth::PrivateToken("tok".into()),
        None,
        Arc::new(TokenBucket::unlimited()),
        ClientOptions {
            max_retries: 1,
            ..fast()
        },
    )
    .unwrap();
    assert!(matches!(
        client.project("1").await,
        Err(CodeHostError::RateLimited { .. })
    ));
    m.with(|s| s.rate_limited = 0);

    let bad = GitLabClient::with_parts(
        &m.url,
        Auth::Bearer("wrong".into()),
        None,
        Arc::new(TokenBucket::unlimited()),
        fast(),
    )
    .unwrap();
    assert_eq!(bad.project("1").await, Err(CodeHostError::Unauthorized));
}

#[tokio::test]
async fn version_is_cached_and_token_scopes_checked() {
    let m = mock().await;
    let s = source(&m, r#", "projects": [{"id": 1}]"#);
    assert_eq!(s.client().version().await.unwrap().major, 16);
    assert_eq!(s.client().version().await.unwrap().minor, 5);
    assert_eq!(
        m.requests()
            .iter()
            .filter(|r| *r == "/api/v4/version")
            .count(),
        1
    );
    let report = s.check().await.unwrap();
    assert!(report.warnings.is_empty());
    assert_eq!(report.account.as_deref(), Some("root"));
    m.with(|s| s.scopes = Some(vec!["read_user".into()]));
    let report = s.check().await.unwrap();
    assert_eq!(report.scopes, Some(vec!["read_user".to_owned()]));
    let warnings = report.warnings;
    assert!(warnings[0].contains("read_api"), "{warnings:?}");

    // Old instances without keyset support get offset pages.
    m.with(|s| s.version = Some("12.9.0".into()));
    let old = source(&m, r#", "projectQuery": ["projects?membership=true"]"#);
    assert_eq!(list(&old, ListOptions::default()).await.len(), 5);
    assert!(
        m.requests()
            .iter()
            .any(|r| r.starts_with("/api/v4/projects?") && r.contains("page=2"))
    );
    assert!(
        old.check().await.unwrap().warnings.is_empty(),
        "no token check before 15.5"
    );
}
