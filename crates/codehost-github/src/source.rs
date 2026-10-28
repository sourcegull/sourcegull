//! Repository discovery for a GitHub connection.
//!
//! A listing resolves `repos` (by `owner/name`), lists the repositories of each of `orgs`,
//! runs each `repositoryQuery` (`affiliated`, `public` or a search query; with `githubApp`,
//! `affiliated` lists the repositories of the App's installation), drops duplicates
//! and `exclude`d repositories, and names each one with `repositoryPathPattern` and
//! `nameTransformations`. Every listing is full: GitHub's list endpoints cannot filter by
//! activity.
//!
//! A repository's external id is its numeric id, which survives renames and transfers.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use regex::Regex;
use sourcegull_codehost::{
    CheckReport, CodeHostError, Credential, ListOptions, RepoMeta, RepoSource, RepoStream, Result,
};
use sourcegull_types::{ExternalRepoSpec, Visibility};
use url::Url;

use crate::client::{GitHubClient, PER_PAGE};
use crate::config::{
    Exclude, GitHubConnection, GitUrlType, QUERY_AFFILIATED, QUERY_NONE, QUERY_PUBLIC,
};
use crate::types::{Repository, RepositorySearch};

/// The service type of GitHub repos (`repo.external_service_type`).
pub const SERVICE_TYPE: &str = "github";

/// The username sent with the token for HTTP clones (GitHub ignores it for personal access
/// tokens and requires it for installation tokens).
pub const CLONE_USERNAME: &str = "x-access-token";

struct CompiledExclude {
    name: Option<String>,
    id: Option<i64>,
    pattern: Option<Regex>,
    archived: bool,
    forks: bool,
}

impl CompiledExclude {
    fn matches(&self, r: &Repository) -> bool {
        self.name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(&r.full_name))
            || self.id == Some(r.id)
            || self
                .pattern
                .as_ref()
                .is_some_and(|p| p.is_match(&r.full_name))
            || (self.archived && r.archived)
            || (self.forks && r.fork)
    }
}

/// Lists the repositories of one GitHub connection.
pub struct GitHubSource {
    conn: GitHubConnection,
    client: Arc<GitHubClient>,
    service_id: String,
    host: String,
    excludes: Vec<CompiledExclude>,
    transformations: Vec<(Regex, String)>,
}

impl std::fmt::Debug for GitHubSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubSource")
            .field("service_id", &self.service_id)
            .finish_non_exhaustive()
    }
}

/// One listing request of a connection.
enum Query {
    /// A list endpoint answering a JSON array.
    List(String),
    /// A repository search.
    Search(String),
    /// Every public repository (`GET /repositories`).
    Public,
    /// The repositories of the GitHub App installation (`GET /installation/repositories`).
    Installation,
}

/// A page of `GET /installation/repositories`.
#[derive(serde::Deserialize)]
struct InstallationRepositories {
    repositories: Vec<Repository>,
}

impl GitHubSource {
    /// A source for a validated connection.
    pub fn new(conn: GitHubConnection, client: Arc<GitHubClient>) -> Result<Self> {
        let base = client.base_url().clone();
        let host = match base.port() {
            Some(port) => format!("{}:{port}", base.host_str().unwrap_or_default()),
            None => base.host_str().unwrap_or_default().to_owned(),
        };
        let excludes = conn
            .exclude
            .iter()
            .map(|e: &Exclude| {
                Ok(CompiledExclude {
                    name: e.name.clone(),
                    id: e.id,
                    pattern: e
                        .pattern
                        .as_deref()
                        .map(Regex::new)
                        .transpose()
                        .map_err(|e| CodeHostError::Config(format!("exclude pattern: {e}")))?,
                    archived: e.archived.unwrap_or(false),
                    forks: e.forks.unwrap_or(false),
                })
            })
            .collect::<Result<_>>()?;
        let transformations = conn
            .name_transformations
            .iter()
            .map(|t| {
                Regex::new(&t.regex)
                    .map(|r| (r, t.replacement.clone()))
                    .map_err(|e| CodeHostError::Config(format!("nameTransformations: {e}")))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            service_id: base.to_string(),
            conn,
            client,
            host,
            excludes,
            transformations,
        })
    }

    /// The API client.
    pub fn client(&self) -> &Arc<GitHubClient> {
        &self.client
    }

    /// Whether `exclude` drops the repository.
    pub fn is_excluded(&self, r: &Repository) -> bool {
        self.excludes.iter().any(|e| e.matches(r))
    }

    /// The Sourcegull repo name of a repository.
    pub fn repo_name(&self, r: &Repository) -> String {
        let mut name = self
            .conn
            .repository_path_pattern
            .replace("{host}", &self.host)
            .replace("{nameWithOwner}", &r.full_name);
        for (re, replacement) in &self.transformations {
            name = re.replace_all(&name, replacement.as_str()).into_owned();
        }
        name
    }

    /// Converts a repository to [`RepoMeta`].
    pub fn repo_meta(&self, r: &Repository) -> RepoMeta {
        let visibility = match r.visibility() {
            "public" => Visibility::Public,
            "internal" => Visibility::Internal,
            _ => Visibility::Private,
        };
        let clone_url = match self.conn.git_url_type {
            GitUrlType::Http => r.clone_url.clone(),
            GitUrlType::Ssh => r.ssh_url.clone(),
        };
        let metadata = serde_json::json!({
            "id": r.id,
            "nodeId": r.node_id,
            "nameWithOwner": r.full_name,
            "forksCount": r.forks_count,
            "cloneUrl": r.clone_url,
            "createdAt": r.created_at,
            "pushedAt": r.pushed_at,
            "disabled": r.disabled,
            "viewerPermissions": r.permissions,
        });
        RepoMeta {
            external: ExternalRepoSpec {
                id: r.id.to_string(),
                service_type: SERVICE_TYPE.to_owned(),
                service_id: self.service_id.clone(),
            },
            name: self.repo_name(r),
            uri: format!("{}/{}", self.host, r.full_name),
            url: r.html_url.clone(),
            description: r.description.clone().unwrap_or_default(),
            fork: r.fork,
            archived: r.archived,
            private: match visibility {
                Visibility::Public => false,
                Visibility::Internal => !self.conn.mark_internal_repos_as_public,
                Visibility::Private => true,
            },
            visibility,
            stars: i32::try_from(r.stargazers_count).unwrap_or(i32::MAX),
            topics: r.topics.clone(),
            default_branch: r.default_branch.clone(),
            last_activity_at: r.last_activity_at(),
            empty: r.size == 0,
            clone_url,
            metadata,
        }
    }

    /// The git credential of HTTP clones with `token`; none for SSH clones.
    fn http_credential(&self, token: Option<String>) -> Credential {
        match (self.conn.git_url_type, token) {
            (GitUrlType::Http, Some(password)) => Credential::Basic {
                username: CLONE_USERNAME.to_owned(),
                password,
            },
            _ => Credential::None,
        }
    }

    /// [`RepoSource::check`] of a GitHub App connection: mints an installation token and
    /// lists one repository of the installation (installation tokens have no user).
    async fn check_app(&self) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        if let Some(app) = &self.conn.github_app {
            report.account = Some(format!(
                "GitHub App {} (installation {})",
                app.app_id, app.installation_id
            ));
        }
        let page: InstallationRepositories = self
            .client
            .get("installation/repositories?per_page=1")
            .await?;
        if page.repositories.is_empty() {
            report
                .warnings
                .push("the GitHub App installation has access to no repositories".to_owned());
        }
        if self.conn.authorization.is_none() {
            report.warnings.push(
                "without an authorization block, every Sourcegull user can see every repository of \
                 this connection, including private ones"
                    .to_owned(),
            );
        }
        Ok(report)
    }

    /// The listing requests of the connection's `orgs` and `repositoryQuery`.
    fn queries(&self) -> Vec<Query> {
        let mut out: Vec<Query> = self
            .conn
            .orgs
            .iter()
            .map(|o| {
                let org: String = url::form_urlencoded::byte_serialize(o.as_bytes()).collect();
                Query::List(format!("orgs/{org}/repos?type=all&per_page={PER_PAGE}"))
            })
            .collect();
        for q in &self.conn.repository_query {
            match q.as_str() {
                QUERY_NONE => {}
                QUERY_AFFILIATED if self.client.is_app() => out.push(Query::Installation),
                QUERY_AFFILIATED => out.push(Query::List(format!(
                    "user/repos?affiliation=owner,collaborator,organization_member&per_page={PER_PAGE}"
                ))),
                // `GET /repositories` pages by `since` (the last id) through `Link` headers and
                // returns minimal objects; each is completed by `repo_by_id`.
                QUERY_PUBLIC => out.push(Query::Public),
                search => out.push(Query::Search(search.to_owned())),
            }
        }
        out
    }

    fn query_url(&self, query: &Query) -> Result<Url> {
        match query {
            Query::List(path) => self.client.api_url(path),
            Query::Public => self.client.api_url("repositories"),
            Query::Installation => self
                .client
                .api_url(&format!("installation/repositories?per_page={PER_PAGE}")),
            Query::Search(q) => {
                let mut url = self.client.api_url("search/repositories")?;
                url.query_pairs_mut()
                    .append_pair("q", q)
                    .append_pair("per_page", &PER_PAGE.to_string());
                Ok(url)
            }
        }
    }

    fn run_query(&self, query: Query) -> RepoStreamOf<'_, Repository> {
        let url = match self.query_url(&query) {
            Ok(u) => u,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match query {
            Query::Search(_) => self
                .client
                .paginate_with(url, |page: RepositorySearch| page.items),
            Query::Public => self
                .client
                .paginate::<MinimalRepository>(url)
                .then(move |item| async move {
                    match item {
                        Ok(m) => self.client.repo_by_id(&m.id.to_string()).await,
                        Err(e) => Err(e),
                    }
                })
                .boxed(),
            Query::List(_) => self.client.paginate::<Repository>(url),
            Query::Installation => self
                .client
                .paginate_with(url, |page: InstallationRepositories| page.repositories),
        }
    }
}

type RepoStreamOf<'a, T> = futures::stream::BoxStream<'a, Result<T>>;

/// An item of `GET /repositories`, which lacks most repository fields.
#[derive(serde::Deserialize)]
struct MinimalRepository {
    id: i64,
}

#[async_trait]
impl RepoSource for GitHubSource {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn list_repos(&self, _opts: ListOptions) -> RepoStream<'_> {
        let explicit = stream::iter(self.conn.repos.clone())
            .then(move |r| async move {
                self.client.repo(&r).await.map_err(|e| match e {
                    CodeHostError::NotFound(_) => {
                        CodeHostError::NotFound(format!("repository {r:?}"))
                    }
                    other => other,
                })
            })
            .boxed();
        let queried = stream::iter(self.queries())
            .map(move |q| self.run_query(q))
            .flatten();
        let seen = Arc::new(Mutex::new(HashSet::<i64>::new()));
        explicit
            .chain(queried)
            .filter_map(move |item| {
                let seen = seen.clone();
                async move {
                    match item {
                        Err(e) => Some(Err(e)),
                        Ok(r) => {
                            let new = seen
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(r.id);
                            (new && !self.is_excluded(&r)).then(|| Ok(self.repo_meta(&r)))
                        }
                    }
                }
            })
            .boxed()
    }

    async fn get_repo(&self, external_id: &str) -> Result<RepoMeta> {
        let r = self.client.repo_by_id(external_id).await?;
        Ok(self.repo_meta(&r))
    }

    fn credential(&self) -> Credential {
        let token = match &self.conn.token {
            Some(t) => Some(t.clone()),
            None => self.client.cached_installation_token(),
        };
        self.http_credential(token)
    }

    async fn current_credential(&self) -> Result<Credential> {
        if self.client.is_app() && self.conn.git_url_type == GitUrlType::Http {
            return Ok(self.http_credential(self.client.installation_token().await?));
        }
        Ok(self.credential())
    }

    async fn check(&self) -> Result<CheckReport> {
        if self.client.is_app() {
            return self.check_app().await;
        }
        let (user, scopes) = self.client.current_user().await?;
        let mut report = CheckReport {
            account: Some(user.login),
            ..CheckReport::default()
        };
        let private_access = match &scopes {
            // A classic token: `repo` reads private repositories; without it only public ones
            // are listed and cloned.
            Some(scopes) => scopes.iter().any(|s| s == "repo"),
            // A fine-grained token's access is per repository and not reported.
            None => true,
        };
        if scopes.as_ref().is_some_and(|_| !private_access) {
            report.warnings.push(
                "the token lacks the repo scope: only public repositories are synced".to_owned(),
            );
        }
        if private_access && self.conn.authorization.is_none() {
            report.warnings.push(
                "without an authorization block, every Sourcegull user can see every repository of \
                 this connection, including private ones"
                    .to_owned(),
            );
        }
        report.scopes = scopes;
        Ok(report)
    }
}
