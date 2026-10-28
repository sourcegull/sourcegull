//! Repository discovery for a Gitea connection.
//!
//! A listing resolves `repos` (by `owner/name`), lists the repositories of each of `orgs`,
//! runs each `repositoryQuery` (`affiliated`, `all` or a search keyword), drops duplicates
//! and `exclude`d repositories, and names each one with `repositoryPathPattern` and
//! `nameTransformations`. Every listing is full: Gitea's list endpoints cannot filter by
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

use crate::client::GiteaClient;
use crate::config::{
    Exclude, GitUrlType, GiteaConnection, QUERY_AFFILIATED, QUERY_ALL, QUERY_NONE,
};
use crate::types::{RepoVisibility, Repository, RepositorySearch};

/// The service type of Gitea repos (`repo.external_service_type`).
pub const SERVICE_TYPE: &str = "gitea";

/// The username sent with the token for HTTP clones (Gitea ignores it when the password is
/// an access token).
pub const CLONE_USERNAME: &str = "sourcegull";

/// The oldest Gitea version Sourcegull supports.
pub const MIN_VERSION: (u32, u32) = (1, 20);

struct CompiledExclude {
    name: Option<String>,
    id: Option<i64>,
    pattern: Option<Regex>,
    archived: bool,
    forks: bool,
    mirrors: bool,
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
            || (self.mirrors && r.mirror)
    }
}

/// Lists the repositories of one Gitea connection.
pub struct GiteaSource {
    conn: GiteaConnection,
    client: Arc<GiteaClient>,
    service_id: String,
    host: String,
    excludes: Vec<CompiledExclude>,
    transformations: Vec<(Regex, String)>,
}

impl std::fmt::Debug for GiteaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiteaSource")
            .field("service_id", &self.service_id)
            .finish_non_exhaustive()
    }
}

/// One listing request of a connection.
enum Query {
    /// A list endpoint answering a JSON array.
    List(String),
    /// A repository search; an empty keyword lists every repository the token can read.
    Search(String),
}

impl GiteaSource {
    /// A source for a validated connection.
    pub fn new(conn: GiteaConnection, client: Arc<GiteaClient>) -> Result<Self> {
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
                    mirrors: e.mirrors.unwrap_or(false),
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
    pub fn client(&self) -> &Arc<GiteaClient> {
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
            RepoVisibility::Public => Visibility::Public,
            RepoVisibility::Internal => Visibility::Internal,
            RepoVisibility::Private => Visibility::Private,
        };
        let clone_url = match self.conn.git_url_type {
            GitUrlType::Http => r.clone_url.clone(),
            GitUrlType::Ssh => r.ssh_url.clone(),
        };
        let metadata = serde_json::json!({
            "id": r.id,
            "nameWithOwner": r.full_name,
            "forksCount": r.forks_count,
            "cloneUrl": r.clone_url,
            "createdAt": r.created_at,
            "mirror": r.mirror,
            "template": r.template,
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
            description: r.description.clone(),
            fork: r.fork,
            archived: r.archived,
            private: visibility != Visibility::Public,
            visibility,
            stars: i32::try_from(r.stars_count).unwrap_or(i32::MAX),
            topics: r.topics.clone(),
            default_branch: Some(r.default_branch.clone()).filter(|b| !b.is_empty()),
            last_activity_at: r.updated_at,
            empty: r.empty,
            clone_url,
            metadata,
        }
    }

    /// The listing requests of the connection's `orgs` and `repositoryQuery`.
    fn queries(&self) -> Vec<Query> {
        let mut out: Vec<Query> = self
            .conn
            .orgs
            .iter()
            .map(|o| {
                let org: String = url::form_urlencoded::byte_serialize(o.as_bytes()).collect();
                Query::List(format!("orgs/{org}/repos"))
            })
            .collect();
        for q in &self.conn.repository_query {
            match q.as_str() {
                QUERY_NONE => {}
                QUERY_AFFILIATED => out.push(Query::List("user/repos".to_owned())),
                QUERY_ALL => out.push(Query::Search(String::new())),
                keyword => out.push(Query::Search(keyword.to_owned())),
            }
        }
        out
    }

    fn query_url(&self, query: &Query) -> Result<Url> {
        match query {
            Query::List(path) => self.client.api_url(path),
            Query::Search(q) => {
                let mut url = self.client.api_url("repos/search")?;
                if !q.is_empty() {
                    url.query_pairs_mut().append_pair("q", q);
                }
                Ok(url)
            }
        }
    }

    fn run_query(&self, query: Query) -> futures::stream::BoxStream<'_, Result<Repository>> {
        let url = match self.query_url(&query) {
            Ok(u) => u,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match query {
            Query::Search(_) => self
                .client
                .paginate_with(url, |page: RepositorySearch| page.data),
            Query::List(_) => self.client.paginate::<Repository>(url),
        }
    }
}

/// `major.minor` of a version such as `1.24.6` or `1.22.0+rc1`.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim_start_matches('v').split(['.', '+', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[async_trait]
impl RepoSource for GiteaSource {
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
        match self.conn.git_url_type {
            GitUrlType::Ssh => Credential::None,
            GitUrlType::Http => Credential::Basic {
                username: CLONE_USERNAME.to_owned(),
                password: self.conn.token.clone(),
            },
        }
    }

    async fn check(&self) -> Result<CheckReport> {
        let user = self.client.current_user().await?;
        let mut report = CheckReport {
            account: Some(user.login),
            ..CheckReport::default()
        };
        // Instances can hide the version from API users; an unknown version is not an error.
        if let Ok(version) = self.client.version().await
            && major_minor(&version).is_some_and(|v| v < MIN_VERSION)
        {
            report.warnings.push(format!(
                "Gitea {version} is older than {}.{}, the oldest version Sourcegull supports",
                MIN_VERSION.0, MIN_VERSION.1
            ));
        }
        report.warnings.push(
            "Gitea repository permissions are not synced: every Sourcegull user can see every \
             repository of this connection, including private ones"
                .to_owned(),
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(major_minor("1.24.6"), Some((1, 24)));
        assert_eq!(major_minor("1.22.0+rc1"), Some((1, 22)));
        assert_eq!(major_minor("v1.19"), Some((1, 19)));
        assert_eq!(major_minor("dev"), None);
        assert!(major_minor("1.19.4").unwrap() < MIN_VERSION);
    }
}
