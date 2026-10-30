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
use futures::FutureExt;
use futures::future::BoxFuture;
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
        self.check_selection(&mut report).await?;
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
            .map(|o| Query::List(org_repos_path(o, PER_PAGE)))
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

/// The path listing the repositories of organization `org`.
fn org_repos_path(org: &str, per_page: u32) -> String {
    let org: String = url::form_urlencoded::byte_serialize(org.as_bytes()).collect();
    format!("orgs/{org}/repos?type=all&per_page={per_page}")
}

/// What checking one entry of the repository selection found.
enum Finding {
    Fine,
    /// The sync works but likely not as intended.
    Warning(String),
    /// The sync would fail.
    Problem(String),
}

/// How many selection entries are checked at once.
const CHECK_CONCURRENCY: usize = 8;

impl GitHubSource {
    /// Checks `orgs`, `repos` and the search queries of `repositoryQuery` with the requests
    /// a sync makes. Every entry a sync would fail on is named in one
    /// [`CodeHostError::Config`]; entries that would sync nothing become warnings.
    async fn check_selection(&self, report: &mut CheckReport) -> Result<()> {
        let mut checks: Vec<BoxFuture<'_, Result<Finding>>> = Vec::new();
        for org in &self.conn.orgs {
            checks.push(self.check_org(org).boxed());
        }
        for repo in &self.conn.repos {
            checks.push(self.check_repo(repo).boxed());
        }
        for q in &self.conn.repository_query {
            if ![QUERY_NONE, QUERY_AFFILIATED, QUERY_PUBLIC].contains(&q.as_str()) {
                checks.push(self.check_search(q).boxed());
            }
        }
        let findings: Vec<Result<Finding>> = stream::iter(checks)
            .buffered(CHECK_CONCURRENCY)
            .collect()
            .await;
        let mut problems = Vec::new();
        for f in findings {
            match f? {
                Finding::Fine => {}
                Finding::Warning(w) => report.warnings.push(w),
                Finding::Problem(p) => problems.push(p),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(CodeHostError::Config(problems.join("; ")))
        }
    }

    async fn check_org(&self, org: &str) -> Result<Finding> {
        let account = match self.client.account(org).await {
            Ok(a) => a,
            Err(CodeHostError::NotFound(_)) => {
                return Ok(Finding::Problem(format!(
                    "organization {org:?} does not exist"
                )));
            }
            Err(e) => return Err(e),
        };
        if !account.is_organization() {
            return Ok(Finding::Problem(format!(
                "{org:?} is a user account, not an organization: list its repositories as \
                 repositories, or mirror the token user's own repositories (\"{QUERY_AFFILIATED}\")"
            )));
        }
        match self
            .client
            .get::<Vec<Repository>>(&org_repos_path(org, 1))
            .await
        {
            Ok(page) if page.is_empty() => Ok(Finding::Warning(format!(
                "the token can read no repositories of organization {org:?}"
            ))),
            Ok(_) => Ok(Finding::Fine),
            Err(CodeHostError::NotFound(_) | CodeHostError::Forbidden(_)) => Ok(Finding::Problem(
                format!("the token cannot list the repositories of organization {org:?}"),
            )),
            Err(e) => Err(e),
        }
    }

    async fn check_repo(&self, name: &str) -> Result<Finding> {
        match self.client.repo(name).await {
            Ok(_) => Ok(Finding::Fine),
            Err(CodeHostError::NotFound(_) | CodeHostError::Forbidden(_)) => Ok(Finding::Problem(
                format!("repository {name:?} does not exist or the token cannot read it"),
            )),
            Err(e) => Err(e),
        }
    }

    async fn check_search(&self, query: &str) -> Result<Finding> {
        let mut url = self.client.api_url("search/repositories")?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("per_page", "1");
        match self.client.get_page::<RepositorySearch>(url).await {
            Ok((r, _)) if r.items.is_empty() => Ok(Finding::Warning(format!(
                "search query {query:?} matches no repositories"
            ))),
            Ok(_) => Ok(Finding::Fine),
            Err(CodeHostError::Http {
                status: 422,
                message,
            }) => Ok(Finding::Problem(format!(
                "search query {query:?} is invalid: {}",
                error_message(&message)
            ))),
            Err(e) => Err(e),
        }
    }
}

/// The text of a GitHub error body: the messages of its `errors`, else its `message`, else
/// the body as it is.
fn error_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_owned();
    };
    let errors: Vec<&str> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str())
        .collect();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    v["message"].as_str().unwrap_or(body).to_owned()
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
        let reaches_orgs = self
            .conn
            .repository_query
            .iter()
            .any(|q| q == QUERY_AFFILIATED)
            || !self.conn.orgs.is_empty();
        if scopes.is_none() && reaches_orgs {
            report.warnings.push(
                "this is a fine-grained token: it reads only the repositories of its resource \
                 owner, so repositories of other organizations are not listed (in orgs, only \
                 their public ones). Use a classic token with the repo scope to reach every \
                 organization of the user"
                    .to_owned(),
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
        self.check_selection(&mut report).await?;
        Ok(report)
    }
}
