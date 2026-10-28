//! Tests against a real Gitea, to catch differences between the mock and Gitea. Run with an
//! access token (scopes `read:repository`, `read:organization` and `read:user`):
//! `SOURCEGULL_TEST_GITEA_TOKEN=<token> scripts/dev cargo test -p sourcegull-codehost-gitea --test
//! live`. The instance is `SOURCEGULL_TEST_GITEA_URL` (default `https://gitea.com`) and the
//! repository `SOURCEGULL_TEST_GITEA_REPO` (default `gitea/tea`), whose owner must be an
//! organization. Without `SOURCEGULL_TEST_GITEA_TOKEN` every test passes without doing anything;
//! CI does not set it.

use std::sync::Arc;

use futures::TryStreamExt;
use sourcegull_codehost::{ClientOptions, ListOptions, RepoMeta, RepoSource};
use sourcegull_codehost_gitea::{GiteaClient, GiteaSource, validate_connection};

struct Live {
    url: String,
    repo: String,
}

impl Live {
    fn host(&self) -> String {
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    u.port()
                        .map_or_else(|| h.to_owned(), |p| format!("{h}:{p}"))
                })
            })
            .unwrap_or_default()
    }

    fn org(&self) -> &str {
        self.repo.split('/').next().unwrap_or_default()
    }
}

fn source(extra: impl Fn(&Live) -> String) -> Option<(Live, GiteaSource)> {
    let token = std::env::var("SOURCEGULL_TEST_GITEA_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())?;
    let env = |k: &str, default: &str| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_owned())
    };
    let live = Live {
        url: env("SOURCEGULL_TEST_GITEA_URL", "https://gitea.com"),
        repo: env("SOURCEGULL_TEST_GITEA_REPO", "gitea/tea"),
    };
    let text = format!(
        r#"{{"url": "{}", "token": "{token}" {}}}"#,
        live.url,
        extra(&live)
    );
    let conn = validate_connection(&text).unwrap().config;
    let client = Arc::new(GiteaClient::new(&conn, ClientOptions::default()).unwrap());
    Some((live, GiteaSource::new(conn, client).unwrap()))
}

#[tokio::test]
async fn lists_a_repository_and_checks_the_token() {
    let Some((live, s)) = source(|l| format!(r#", "repos": ["{}"]"#, l.repo)) else {
        return;
    };
    let repos: Vec<RepoMeta> = s
        .list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(repos.len(), 1);
    let r = &repos[0];
    assert_eq!(r.name, format!("{}/{}", live.host(), live.repo));
    assert!(r.clone_url.ends_with(&format!("/{}.git", live.repo)));
    // The external id is stable and fetchable.
    let again = s.get_repo(&r.external.id).await.unwrap();
    assert_eq!(again.name, r.name);

    let report = s.check().await.unwrap();
    assert!(report.account.is_some());
}

#[tokio::test]
async fn paginates_an_organization_and_a_search() {
    let Some((live, s)) = source(|l| {
        let name = l.repo.split('/').nth(1).unwrap_or_default();
        format!(
            r#", "orgs": ["{}"], "repositoryQuery": ["{name}"]"#,
            l.org()
        )
    }) else {
        return;
    };
    let repos: Vec<RepoMeta> = s
        .list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap();
    let prefix = format!("{}/{}/", live.host(), live.org());
    assert!(repos.iter().any(|r| r.name.starts_with(&prefix)));
    assert!(
        repos
            .iter()
            .any(|r| r.name == format!("{}/{}", live.host(), live.repo))
    );
    // No duplicates although the search finds a repository of the organization.
    let mut ids: Vec<&str> = repos.iter().map(|r| r.external.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), repos.len());
}
