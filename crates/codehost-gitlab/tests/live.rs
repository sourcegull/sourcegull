//! Tests against the real development GitLab (`scripts/dev gitlab up`, seeded by
//! deploy/docker/dev/gitlab-seed.rb), to catch differences between the mock and GitLab.
//! Run with `scripts/dev gitlab test`, or on the host (with `127.0.0.1 gitlab.localhost` in
//! /etc/hosts: GitLab's pagination links use that name, and the client sends the token only to
//! the configured host) with `SOURCEGULL_TEST_GITLAB_URL=http://gitlab.localhost:8929 cargo test
//! -p sourcegull-codehost-gitlab --test integration live::`.
//! Without `SOURCEGULL_TEST_GITLAB_URL` every test passes without doing anything; CI does not set it.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::TryStreamExt;
use sourcegull_codehost::{AuthzProvider, ExternalAccountData, ListOptions, RepoMeta, RepoSource};
use sourcegull_codehost_gitlab::{
    ClientOptions, GitLabAuthzProvider, GitLabClient, GitLabConnection, GitLabOAuth, GitLabSource,
    validate_connection,
};
use sourcegull_types::Visibility;

/// The seed's root token (api, sudo).
const ROOT_TOKEN: &str = "glpat-sourcegull-dev-root";

fn url() -> Option<String> {
    let url = std::env::var("SOURCEGULL_TEST_GITLAB_URL").ok()?;
    (!url.is_empty()).then_some(url)
}

fn user_token(name: &str) -> String {
    format!("glpat-sourcegull-dev-{name}")
}

fn connection(url: &str, extra: &str) -> GitLabConnection {
    let text = format!(r#"{{"url": "{url}", "token": "{ROOT_TOKEN}" {extra}}}"#);
    validate_connection(&text).unwrap().config
}

fn client(conn: &GitLabConnection) -> Arc<GitLabClient> {
    Arc::new(GitLabClient::new(conn, ClientOptions::default()).unwrap())
}

fn source(url: &str, extra: &str) -> GitLabSource {
    let conn = connection(url, extra);
    GitLabSource::new(conn.clone(), client(&conn)).unwrap()
}

async fn list(s: &GitLabSource) -> Vec<RepoMeta> {
    s.list_repos(ListOptions::default())
        .try_collect()
        .await
        .unwrap()
}

/// `group/project` paths of `repos` (repo names without the host).
fn paths(repos: &[RepoMeta]) -> BTreeSet<String> {
    repos
        .iter()
        .map(|r| r.name.split_once('/').unwrap().1.to_owned())
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// The numeric ids (external ids) of seeded projects.
async fn ids(url: &str, projects: &[&str]) -> BTreeSet<String> {
    let c = client(&connection(url, ""));
    let mut out = BTreeSet::new();
    for p in projects {
        out.insert(c.project(p).await.unwrap().id.to_string());
    }
    out
}

fn authz(url: &str, identity: &str) -> GitLabAuthzProvider {
    let conn = connection(
        url,
        &format!(r#", "authorization": {{"identityProvider": {{"type": "{identity}"}}}}"#),
    );
    GitLabAuthzProvider::new(&conn, client(&conn))
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn version_and_token_scopes() {
    let Some(url) = url() else { return };
    let c = client(&connection(&url, ""));
    assert!(c.version().await.is_some(), "version");
    let scopes = c.token_scopes().await.unwrap().expect("scopes");
    assert!(scopes.iter().any(|s| s == "sudo"), "{scopes:?}");
}

#[tokio::test]
async fn discovery_of_group_projects_with_keyset_pages() {
    let Some(url) = url() else { return };
    // per_page=2 makes every listing span several pages.
    let s = source(
        &url,
        r#", "projectQuery": ["groups/acme/projects?include_subgroups=true&per_page=2"]"#,
    );
    let repos = list(&s).await;
    assert_eq!(
        paths(&repos),
        set(&[
            "acme/api",
            "acme/web",
            "acme/empty",
            "acme/legacy",
            "acme/handbook",
            "acme/platform/tools",
        ])
    );
    let get = |p: &str| repos.iter().find(|r| r.name.ends_with(p)).unwrap();
    assert!(get("/acme/empty").empty);
    assert!(!get("/acme/api").empty);
    assert!(get("/acme/legacy").archived);
    assert_eq!(get("/acme/handbook").visibility, Visibility::Internal);
    assert!(get("/acme/api").private);
    assert_eq!(get("/acme/api").default_branch.as_deref(), Some("main"));
    assert!(get("/acme/api").clone_url.ends_with("/acme/api.git"));
}

#[tokio::test]
async fn discovery_of_all_projects_and_explicit_ones() {
    let Some(url) = url() else { return };
    let s = source(&url, r#", "projectQuery": ["projects?per_page=3"]"#);
    let all = paths(&list(&s).await);
    assert!(
        all.is_superset(&set(&["acme/api", "community/docs"])),
        "{all:?}"
    );

    let s = source(
        &url,
        r#", "projectQuery": ["none"], "projects": [{"name": "community/docs"}],
            "exclude": [{"emptyRepos": true}]"#,
    );
    assert_eq!(paths(&list(&s).await), set(&["community/docs"]));
}

#[tokio::test]
async fn user_perms_with_sudo() {
    let Some(url) = url() else { return };
    let p = authz(&url, "username");
    let perms = |name: &'static str| {
        let p = p.clone();
        async move {
            let account = p.fetch_account(name, &[]).await.unwrap().expect(name);
            let ids: BTreeSet<_> = p
                .fetch_user_perms(&account)
                .await
                .unwrap()
                .external_ids
                .into_iter()
                .collect();
            ids
        }
    };
    // alice: direct developer on api and web, developer of the acme/platform group (tools).
    assert_eq!(
        perms("alice").await,
        ids(&url, &["acme/api", "acme/web", "acme/platform/tools"]).await
    );
    assert_eq!(perms("bob").await, ids(&url, &["acme/api"]).await);
    // carol is only a guest on tools, which cannot read code.
    assert!(perms("carol").await.is_empty());
    assert!(p.fetch_account("nobody-here", &[]).await.unwrap().is_none());
}

#[tokio::test]
async fn user_perms_with_the_users_token() {
    let Some(url) = url() else { return };
    let p = authz(&url, "oauth");
    // A personal access token works as a bearer token, like an OAuth token.
    let account = ExternalAccountData {
        account_id: String::new(),
        username: "bob".into(),
        access_token: Some(user_token("bob")),
        expires_at: None,
    };
    let ids_: BTreeSet<_> = p
        .fetch_user_perms(&account)
        .await
        .unwrap()
        .external_ids
        .into_iter()
        .collect();
    assert_eq!(ids_, ids(&url, &["acme/api"]).await);
}

#[tokio::test]
async fn repo_perms_include_direct_and_inherited_members() {
    let Some(url) = url() else { return };
    let p = authz(&url, "username");
    let names = |accounts: Vec<ExternalAccountData>| -> BTreeSet<String> {
        accounts.into_iter().map(|a| a.username).collect()
    };
    let api = ids(&url, &["acme/api"]).await.pop_first().unwrap();
    let members = names(p.fetch_repo_perms(&api).await.unwrap());
    assert!(members.is_superset(&set(&["alice", "bob"])), "{members:?}");
    // Inherited from the acme/platform group; carol (guest) cannot read code.
    let tools = ids(&url, &["acme/platform/tools"])
        .await
        .pop_first()
        .unwrap();
    let members = names(p.fetch_repo_perms(&tools).await.unwrap());
    assert!(members.contains("alice"), "{members:?}");
    assert!(!members.contains("carol"), "{members:?}");
}

#[tokio::test]
async fn oauth_user_groups() {
    let Some(url) = url() else { return };
    let oauth = GitLabOAuth::new(
        &url,
        "sourcegull-dev",
        "sourcegull-dev-secret",
        "http://localhost:7080",
    )
    .unwrap();
    let groups = oauth.user_groups(&user_token("alice")).await.unwrap();
    assert!(groups.iter().any(|g| g == "acme/platform"), "{groups:?}");
}
