//! Repository discovery for a GitLab connection.
//!
//! A listing resolves `projects` (by id or path), runs each `projectQuery`, drops duplicates
//! and `exclude`d projects, and names each project with `repositoryPathPattern` and
//! `nameTransformations`.
//!
//! Incremental listings ([`ListOptions::since`]) skip the explicit `projects` and add
//! `last_activity_after` to every query, so only projects with new activity come back. Queries
//! on `projects` use keyset pagination (`pagination=keyset&order_by=id&sort=asc`) when the
//! instance supports it and the query does not choose its own ordering; everything else uses
//! offset pages. Both follow `Link` headers.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::SecondsFormat;
use futures::stream::{self, StreamExt};
use regex::Regex;
use sourcegull_codehost::{
    CheckReport, CodeHostError, Credential, ListOptions, RepoMeta, RepoSource, RepoStream, Result,
};
use sourcegull_types::{ExternalRepoSpec, Visibility};
use url::Url;

use crate::client::{GitLabClient, PER_PAGE};
use crate::config::{
    DEFAULT_PROJECT_QUERY, Exclude, GitLabConnection, GitUrlType, ProjectRef, TokenType,
};
use crate::types::Project;

/// `GET /user`, the fields the connection check reads.
#[derive(serde::Deserialize)]
struct CurrentUser {
    username: String,
}

/// The service type of GitLab repos (`repo.external_service_type`).
pub const SERVICE_TYPE: &str = "gitlab";

/// How far an incremental listing reaches back before the watermark, to tolerate clock skew
/// and GitLab's lazy `last_activity_at` updates.
pub const INCREMENTAL_OVERLAP: chrono::Duration = chrono::Duration::minutes(5);

struct CompiledExclude {
    name: Option<String>,
    id: Option<i64>,
    pattern: Option<Regex>,
    empty_repos: bool,
}

impl CompiledExclude {
    fn matches(&self, p: &Project) -> bool {
        self.name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(&p.path_with_namespace))
            || self.id == Some(p.id)
            || self
                .pattern
                .as_ref()
                .is_some_and(|r| r.is_match(&p.path_with_namespace))
            || (self.empty_repos && p.empty_repo)
    }
}

/// Lists the projects of one GitLab connection.
pub struct GitLabSource {
    conn: GitLabConnection,
    client: Arc<GitLabClient>,
    service_id: String,
    host: String,
    excludes: Vec<CompiledExclude>,
    transformations: Vec<(Regex, String)>,
}

impl std::fmt::Debug for GitLabSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabSource")
            .field("service_id", &self.service_id)
            .finish_non_exhaustive()
    }
}

impl GitLabSource {
    /// A source for a validated connection.
    pub fn new(conn: GitLabConnection, client: Arc<GitLabClient>) -> Result<Self> {
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
                    empty_repos: e.empty_repos.unwrap_or(false),
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
    pub fn client(&self) -> &Arc<GitLabClient> {
        &self.client
    }

    /// Whether `exclude` drops the project.
    pub fn is_excluded(&self, p: &Project) -> bool {
        self.excludes.iter().any(|e| e.matches(p))
    }

    /// The Sourcegull repo name of a project.
    pub fn repo_name(&self, p: &Project) -> String {
        let mut name = self
            .conn
            .repository_path_pattern
            .replace("{host}", &self.host)
            .replace("{pathWithNamespace}", &p.path_with_namespace);
        for (re, replacement) in &self.transformations {
            name = re.replace_all(&name, replacement.as_str()).into_owned();
        }
        name
    }

    /// Converts a project to [`RepoMeta`].
    pub fn repo_meta(&self, p: &Project) -> RepoMeta {
        let visibility = match p.visibility.as_str() {
            "public" => Visibility::Public,
            "internal" => Visibility::Internal,
            _ => Visibility::Private,
        };
        let private = match visibility {
            Visibility::Public => false,
            Visibility::Internal => !self.conn.mark_internal_repos_as_public,
            Visibility::Private => true,
        };
        let topics = if p.topics.is_empty() {
            p.tag_list.clone()
        } else {
            p.topics.clone()
        };
        let clone_url = match self.conn.git_url_type {
            GitUrlType::Http => p.http_url_to_repo.clone(),
            GitUrlType::Ssh => p.ssh_url_to_repo.clone(),
        };
        let metadata = serde_json::json!({
            "id": p.id,
            "pathWithNamespace": p.path_with_namespace,
            "forksCount": p.forks_count,
            "httpUrlToRepo": p.http_url_to_repo,
            "createdAt": p.created_at,
            "emptyRepo": p.empty_repo,
            // Read by the repo node to tell a disabled repository and a token below Reporter
            // apart when a fetch is refused.
            "repositoryAccessLevel": p.repository_access_level,
            "accessLevel": p.access_level(),
            "forkedFromProject": p.forked_from_project.as_ref().map(|f| serde_json::json!({
                "id": f.id,
                "pathWithNamespace": f.path_with_namespace,
            })),
        });
        RepoMeta {
            external: ExternalRepoSpec {
                id: p.id.to_string(),
                service_type: SERVICE_TYPE.to_owned(),
                service_id: self.service_id.clone(),
            },
            name: self.repo_name(p),
            uri: format!("{}/{}", self.host, p.path_with_namespace),
            url: p.web_url.clone(),
            description: p.description.clone().unwrap_or_default(),
            fork: p.forked_from_project.is_some(),
            archived: p.archived,
            private,
            visibility,
            stars: i32::try_from(p.star_count).unwrap_or(i32::MAX),
            topics,
            default_branch: p.default_branch.clone(),
            last_activity_at: p.last_activity_at,
            empty: p.empty_repo,
            clone_url,
            metadata,
        }
    }

    /// The git credential carrying `token` (none for SSH clones).
    fn http_credential(&self, token: String) -> Credential {
        match self.conn.git_url_type {
            GitUrlType::Ssh => Credential::None,
            GitUrlType::Http => Credential::Basic {
                username: match self.conn.token_type {
                    TokenType::Pat => "git".to_owned(),
                    TokenType::Oauth => "oauth2".to_owned(),
                },
                password: token,
            },
        }
    }

    /// The first-page URL of a `projectQuery` entry.
    async fn query_url(
        &self,
        query: &str,
        since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Url> {
        let mut url = self.client.api_url(query)?;
        let existing: HashSet<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        let is_projects = url
            .path()
            .trim_end_matches('/')
            .ends_with("/api/v4/projects");
        let keyset = is_projects
            && !["order_by", "sort", "page", "pagination", "id_after"]
                .iter()
                .any(|k| existing.contains(*k))
            && self
                .client
                .version()
                .await
                .is_none_or(|v| v.supports_keyset_projects());
        {
            let mut pairs = url.query_pairs_mut();
            if !existing.contains("per_page") {
                pairs.append_pair("per_page", &PER_PAGE.to_string());
            }
            if keyset {
                pairs
                    .append_pair("pagination", "keyset")
                    .append_pair("order_by", "id")
                    .append_pair("sort", "asc");
            }
            if let Some(since) = since
                && !existing.contains("last_activity_after")
            {
                let after =
                    (since - INCREMENTAL_OVERLAP).to_rfc3339_opts(SecondsFormat::Secs, true);
                pairs.append_pair("last_activity_after", &after);
            }
        }
        Ok(url)
    }
}

#[async_trait]
impl RepoSource for GitLabSource {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn supports_incremental(&self) -> bool {
        true
    }

    fn list_repos(&self, opts: ListOptions) -> RepoStream<'_> {
        let since = opts.since;
        let explicit = if since.is_some() {
            Vec::new()
        } else {
            self.conn.projects.clone()
        };
        let explicit = stream::iter(explicit)
            .then(move |p| async move {
                let key = match &p {
                    ProjectRef::Name { name } => name.clone(),
                    ProjectRef::Id { id } => id.to_string(),
                };
                self.client.project(&key).await.map_err(|e| match e {
                    CodeHostError::NotFound(_) => {
                        CodeHostError::NotFound(format!("project {key:?}"))
                    }
                    other => other,
                })
            })
            .boxed();
        let queries: Vec<String> = self
            .conn
            .project_query
            .iter()
            .filter(|q| q.as_str() != DEFAULT_PROJECT_QUERY)
            .cloned()
            .collect();
        let queried = stream::iter(queries)
            .then(move |q| async move {
                match self.query_url(&q, since).await {
                    Ok(url) => self.client.paginate::<Project>(url),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                }
            })
            .flatten();
        let seen = Arc::new(Mutex::new(HashSet::<i64>::new()));
        explicit
            .chain(queried)
            .filter_map(move |item| {
                let seen = seen.clone();
                async move {
                    match item {
                        Err(e) => Some(Err(e)),
                        Ok(p) => {
                            let new = seen
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(p.id);
                            (new && !self.is_excluded(&p)).then(|| Ok(self.repo_meta(&p)))
                        }
                    }
                }
            })
            .boxed()
    }

    async fn get_repo(&self, external_id: &str) -> Result<RepoMeta> {
        let p = self.client.project(external_id).await?;
        Ok(self.repo_meta(&p))
    }

    fn credential(&self) -> Credential {
        let token = self
            .client
            .token_session()
            .map_or_else(|| self.conn.token.clone(), |s| s.current().access_token);
        self.http_credential(token)
    }

    async fn current_credential(&self) -> Result<Credential> {
        match self.client.token_session() {
            Some(s) if self.conn.git_url_type == GitUrlType::Http => {
                Ok(self.http_credential(s.access_token().await?))
            }
            _ => Ok(self.credential()),
        }
    }

    async fn check(&self) -> Result<CheckReport> {
        let mut report = CheckReport::default();
        if let Some(scopes) = self.client.token_scopes().await? {
            if !scopes.iter().any(|s| s == "api" || s == "read_api") {
                report.warnings.push(format!(
                    "the token lacks the api or read_api scope (it has: {}); discovery will fail",
                    scopes.join(", ")
                ));
            }
            report.scopes = Some(scopes);
        }
        // Who the token acts as; this also surfaces a bad token or URL as an error. Tokens
        // without a user-reading scope answer 403: probe a cheap endpoint instead.
        match self.client.get::<CurrentUser>("user").await {
            Ok(u) => report.account = Some(u.username),
            Err(CodeHostError::Forbidden(_)) => {
                let _: serde_json::Value =
                    self.client.get("projects?per_page=1&simple=true").await?;
            }
            Err(e) => return Err(e),
        }
        Ok(report)
    }
}
