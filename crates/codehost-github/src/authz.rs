//! Repository permissions from GitHub, implementing [`AuthzProvider`].
//!
//! Only `identityProvider: oauth` is supported: GitHub has no way to act as another user, and
//! matching by username alone is unsafe.
//!
//! - **User-centric**: with the user's own OAuth token, `GET /user/repos` with
//!   `affiliation=owner,collaborator,organization_member` (paginated). A classic token whose
//!   `X-OAuth-Scopes` lack `repo` is [`CodeHostError::MissingScope`]: such a token cannot list
//!   private repositories, and the user has to sign in again.
//! - **Repo-centric**: with the connection token, `GET /repos/{owner}/{repo}/collaborators`
//!   with `affiliation=all` (direct collaborators, team members and organization members
//!   through the base permission), keeping those with `pull` access. GitHub only answers that
//!   for tokens with admin access to the repository, or for a GitHub App installation
//!   (`githubApp`) with the Metadata and Members read permissions; otherwise the sync fails with
//!   [`CodeHostError::Forbidden`] saying so.
//!
//! Only private repositories are listed, plus internal ones (GitHub Enterprise) unless
//! `markInternalReposAsPublic` (public repositories, and internal ones marked public, are
//! readable by everyone anyway). External ids are repository ids, as discovery stores them.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use serde::Deserialize;
use sourcegull_codehost::{AuthzProvider, CodeHostError, ExternalAccountData, Result, UserPerms};

use crate::client::{GitHubClient, PER_PAGE};
use crate::config::{GitHubConnection, IdentityProvider};
use crate::source::SERVICE_TYPE;
use crate::types::{RepoPermissions, Repository};

/// The classic OAuth scope that lets a token read private repositories.
pub const REPO_SCOPE: &str = "repo";

/// The permissions provider of one connection with an `authorization` block.
#[derive(Debug, Clone)]
pub struct GitHubAuthzProvider {
    service_id: String,
    client: Arc<GitHubClient>,
    internal_public: bool,
}

#[derive(Deserialize)]
struct Collaborator {
    id: i64,
    login: String,
    #[serde(default)]
    permissions: Option<RepoPermissions>,
}

impl GitHubAuthzProvider {
    /// The provider of `conn`, or `None` when it has no `authorization` block. `client` is
    /// the connection's API client (its token and rate limiter).
    pub fn new(conn: &GitHubConnection, client: Arc<GitHubClient>) -> Result<Option<Self>> {
        let Some(authz) = &conn.authorization else {
            return Ok(None);
        };
        if authz.identity_provider != IdentityProvider::Oauth {
            return Err(CodeHostError::Config(
                "GitHub supports only the \"oauth\" identity provider".into(),
            ));
        }
        let service_id = conn.base_url().map_err(CodeHostError::Config)?.to_string();
        Ok(Some(Self {
            service_id,
            client,
            internal_public: conn.mark_internal_repos_as_public,
        }))
    }

    /// Whether a repository of this visibility needs permissions.
    fn restricted(&self, r: &Repository) -> bool {
        match r.visibility() {
            "public" => false,
            "internal" => !self.internal_public,
            _ => true,
        }
    }
}

#[async_trait]
impl AuthzProvider for GitHubAuthzProvider {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn service_type(&self) -> &str {
        SERVICE_TYPE
    }

    fn needs_user_token(&self) -> bool {
        true
    }

    fn supports_repo_perms(&self) -> bool {
        true
    }

    async fn fetch_user_perms(&self, account: &ExternalAccountData) -> Result<UserPerms> {
        let token = account
            .access_token
            .as_deref()
            .ok_or(CodeHostError::Unauthorized)?;
        let client = self.client.with_token(token)?;
        // Fine-grained and GitHub App user tokens report no scopes.
        let (_, scopes) = client.current_user().await?;
        if scopes.is_some_and(|s| !s.iter().any(|s| s == REPO_SCOPE)) {
            return Err(CodeHostError::MissingScope(REPO_SCOPE.into()));
        }
        let first = client.api_url(&format!(
            "user/repos?affiliation=owner,collaborator,organization_member&per_page={PER_PAGE}"
        ))?;
        let repos: Vec<Repository> = client.paginate(first).try_collect().await?;
        let mut ids: Vec<String> = repos
            .iter()
            .filter(|r| self.restricted(r))
            .map(|r| r.id.to_string())
            .collect();
        ids.sort();
        ids.dedup();
        Ok(UserPerms { external_ids: ids })
    }

    async fn fetch_repo_perms(&self, external_id: &str) -> Result<Vec<ExternalAccountData>> {
        let repo = self.client.repo_by_id(external_id).await?;
        let path: Vec<String> = repo
            .full_name
            .split('/')
            .map(|s| url::form_urlencoded::byte_serialize(s.as_bytes()).collect())
            .collect();
        let first = self.client.api_url(&format!(
            "repos/{}/collaborators?affiliation=all&per_page={PER_PAGE}",
            path.join("/")
        ))?;
        let collaborators: Vec<Collaborator> = match self.client.paginate(first).try_collect().await
        {
            Ok(c) => c,
            Err(CodeHostError::Forbidden(_) | CodeHostError::NotFound(_)) => {
                return Err(CodeHostError::Forbidden(format!(
                    "listing the collaborators of {} needs a connection token with admin access \
                     to the repository, or a GitHub App installation with the Metadata (read) \
                     repository and Members (read) organization permissions",
                    repo.full_name
                )));
            }
            Err(e) => return Err(e),
        };
        let mut accounts: Vec<ExternalAccountData> = collaborators
            .into_iter()
            .filter(|c| c.permissions.is_none_or(|p| p.pull || p.push || p.admin))
            .map(|c| ExternalAccountData {
                account_id: c.id.to_string(),
                username: c.login,
                access_token: None,
                expires_at: None,
            })
            .collect();
        accounts.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        accounts.dedup_by(|a, b| a.account_id == b.account_id);
        Ok(accounts)
    }

    async fn fetch_account(
        &self,
        _username: &str,
        _verified_emails: &[String],
    ) -> Result<Option<ExternalAccountData>> {
        // Accounts come from signing in with GitHub.
        Ok(None)
    }
}
