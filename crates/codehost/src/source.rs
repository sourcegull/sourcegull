//! Repository discovery: [`RepoSource`] and [`RepoMeta`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use sourcegull_types::{ExternalRepoSpec, Visibility};
use url::Url;

use crate::Result;

/// A repository as a code host describes it, already named for Sourcegull (path pattern and name
/// transformations applied).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoMeta {
    /// Identity on the code host (stable across renames).
    pub external: ExternalRepoSpec,
    /// The Sourcegull repo name, e.g. `gitlab.com/group/project`.
    pub name: String,
    /// `{host}/{pathWithNamespace}`.
    pub uri: String,
    /// The repository's web page.
    pub url: String,
    pub description: String,
    pub fork: bool,
    pub archived: bool,
    /// Not visible to everyone (see [`Visibility`]).
    pub private: bool,
    pub visibility: Visibility,
    pub stars: i32,
    pub topics: Vec<String>,
    pub default_branch: Option<String>,
    /// When the code host last saw activity (pushes, issues, …); drives incremental listing
    /// and activity-triggered fetches.
    pub last_activity_at: Option<DateTime<Utc>>,
    /// The repository has no commits.
    pub empty: bool,
    /// Clone URL without credentials (HTTP(S) or SSH, per the connection's `gitURLType`).
    pub clone_url: String,
    /// Host-specific metadata kept with the repo.
    pub metadata: serde_json::Value,
}

/// Options for [`RepoSource::list_repos`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// Incremental listing: only repos with activity after this time. `None` lists
    /// everything (a full listing, after which repos not returned may be deleted).
    pub since: Option<DateTime<Utc>>,
}

/// Credentials for git operations against a code host.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// No credentials (public repos, or SSH with keys configured on the repo node).
    None,
    /// HTTP basic credentials, e.g. a personal access token with a placeholder username.
    Basic {
        /// The username.
        username: String,
        /// The password or token.
        password: String,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// A stream of discovered repos. An `Err` item reports a partial failure (e.g. one project
/// query failed); the stream may continue, but the listing must then be treated as
/// incomplete (no deletions).
pub type RepoStream<'a> = BoxStream<'a, Result<RepoMeta>>;

/// What [`RepoSource::check`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// The code host account the credentials act as, when the host says.
    pub account: Option<String>,
    /// The token's scopes, when the host reports them.
    pub scopes: Option<Vec<String>>,
    /// Problems that do not prevent syncing.
    pub warnings: Vec<String>,
}

/// Lists the repositories a code host connection covers.
#[async_trait]
pub trait RepoSource: Send + Sync {
    /// The code host instance (`ExternalRepoSpec::service_id` of every repo it lists).
    fn service_id(&self) -> &str;

    /// Lists repos. With [`ListOptions::since`], only repos changed since then are returned
    /// (when the host supports it; see [`supports_incremental`](Self::supports_incremental)).
    fn list_repos(&self, opts: ListOptions) -> RepoStream<'_>;

    /// Whether [`ListOptions::since`] is honoured. Hosts that ignore it always list fully.
    fn supports_incremental(&self) -> bool {
        false
    }

    /// Fetches one repo by its external id.
    async fn get_repo(&self, external_id: &str) -> Result<RepoMeta>;

    /// The connection's own git credential.
    fn credential(&self) -> Credential;

    /// The connection's git credential for a clone or fetch starting now: like
    /// [`credential`](Self::credential), with a token that expires soon refreshed first.
    async fn current_credential(&self) -> Result<Credential> {
        Ok(self.credential())
    }

    /// The URL to clone `repo` with `cred` embedded (for HTTP(S) URLs).
    fn clone_url(&self, repo: &RepoMeta, cred: &Credential) -> Result<Url> {
        let mut url = Url::parse(&repo.clone_url)
            .map_err(|e| crate::CodeHostError::Decode(format!("clone URL: {e}")))?;
        if let Credential::Basic { username, password } = cred
            && matches!(url.scheme(), "http" | "https")
        {
            let bad = |_| crate::CodeHostError::Decode("clone URL cannot carry credentials".into());
            url.set_username(username).map_err(bad)?;
            url.set_password(Some(password)).map_err(bad)?;
        }
        Ok(url)
    }

    /// Checks the connection (reachability, token validity and scopes): who the credentials
    /// act as, and warnings for problems that do not prevent syncing.
    async fn check(&self) -> Result<CheckReport> {
        Ok(CheckReport::default())
    }
}
