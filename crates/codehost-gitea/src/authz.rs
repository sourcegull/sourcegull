//! Repository permissions from Gitea, implementing [`AuthzProvider`].
//!
//! - `identityProvider: oauth` (user-centric only): lists the repositories the user can read
//!   with their own OAuth token (`GET /user/repos`: owned, collaborating, and reached through
//!   organization teams). A `403` there means the token lacks `read:repository`
//!   ([`CodeHostError::MissingScope`]), and the user has to sign in again.
//! - `identityProvider: username`: finds the Gitea user with the Sourcegull username (`GET
//!   /users/{username}`), and lists their repositories with the connection's site
//!   administrator token plus `Sudo: <username>`. Also repo-centric: the repository's
//!   collaborators (`GET /repos/{owner}/{repo}/collaborators`), the members of the
//!   organization teams with access to it (`GET /repos/{owner}/{repo}/teams`, then `GET
//!   /teams/{id}/members` for each team that can read code), and the owner of a repository
//!   owned by a user.
//!
//! "Can read" is read access to the code unit (`permissions.pull` of a listed repository; a
//! team's `units_map["repo.code"]`, or its `permission` when it has no per-unit map). Only
//! repositories that are not public are listed: private ones, and internal ones (owned by a
//! limited organization or user, or marked internal), which the connection's sync marks
//! private too. Public repositories are readable by everyone.

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use serde::Deserialize;
use serde_json::Value;
use sourcegull_codehost::{AuthzProvider, CodeHostError, ExternalAccountData, Result, UserPerms};

use crate::client::GiteaClient;
use crate::config::{GiteaConnection, IdentityProvider};
use crate::oauth::REPO_SCOPE;
use crate::source::SERVICE_TYPE;
use crate::types::{RepoVisibility, Repository};

/// The permissions provider of one connection with an `authorization` block.
#[derive(Debug, Clone)]
pub struct GiteaAuthzProvider {
    service_id: String,
    client: Arc<GiteaClient>,
    identity: IdentityProvider,
}

/// A user object (`GET /users/{username}`, collaborators, team members).
#[derive(Deserialize)]
struct UserRef {
    id: i64,
    login: String,
    /// Shown to site administrators only.
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    prohibit_login: bool,
}

impl UserRef {
    fn usable(&self) -> bool {
        self.active != Some(false) && !self.prohibit_login
    }

    fn account(self) -> ExternalAccountData {
        ExternalAccountData {
            account_id: self.id.to_string(),
            username: self.login,
            access_token: None,
            expires_at: None,
        }
    }
}

/// A team with access to a repository (`GET /repos/{owner}/{repo}/teams`).
#[derive(Deserialize)]
struct Team {
    id: i64,
    /// `none`, `read`, `write`, `admin` or `owner`.
    #[serde(default)]
    permission: Option<String>,
    /// Access per unit, e.g. `{"repo.code": "read"}`.
    #[serde(default)]
    units_map: Option<Value>,
}

/// Whether an access mode reads.
fn reads(mode: &str) -> bool {
    matches!(mode, "read" | "write" | "admin" | "owner")
}

impl Team {
    /// Whether the team's members can read the repository's code.
    fn reads_code(&self) -> bool {
        if self
            .permission
            .as_deref()
            .is_some_and(|p| p == "admin" || p == "owner")
        {
            return true;
        }
        match self
            .units_map
            .as_ref()
            .and_then(|m| m.get("repo.code"))
            .and_then(Value::as_str)
        {
            Some(mode) => reads(mode),
            None => self.permission.as_deref().is_some_and(reads),
        }
    }
}

/// The URL path of `owner/name`, each segment encoded.
fn repo_path(full_name: &str) -> String {
    full_name
        .split('/')
        .map(|s| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>())
        .collect::<Vec<_>>()
        .join("/")
}

impl GiteaAuthzProvider {
    /// The provider of `conn`, or `None` when it has no `authorization` block. `client` is
    /// the connection's API client (its token and rate limiter).
    pub fn new(conn: &GiteaConnection, client: Arc<GiteaClient>) -> Result<Option<Self>> {
        let Some(authz) = &conn.authorization else {
            return Ok(None);
        };
        let service_id = conn.base_url().map_err(CodeHostError::Config)?.to_string();
        Ok(Some(Self {
            service_id,
            client,
            identity: authz.identity_provider.clone(),
        }))
    }
}

#[async_trait]
impl AuthzProvider for GiteaAuthzProvider {
    fn service_id(&self) -> &str {
        &self.service_id
    }

    fn service_type(&self) -> &str {
        SERVICE_TYPE
    }

    fn needs_user_token(&self) -> bool {
        self.identity == IdentityProvider::Oauth
    }

    fn supports_repo_perms(&self) -> bool {
        self.identity == IdentityProvider::Username
    }

    async fn fetch_user_perms(&self, account: &ExternalAccountData) -> Result<UserPerms> {
        let client = match self.identity {
            IdentityProvider::Oauth => {
                let token = account
                    .access_token
                    .as_deref()
                    .ok_or(CodeHostError::Unauthorized)?;
                self.client.with_token(token)?
            }
            IdentityProvider::Username => self.client.with_sudo(&account.username)?,
        };
        let repos: Vec<Repository> = match client
            .paginate(client.api_url("user/repos")?)
            .try_collect()
            .await
        {
            // A user token without the scope cannot list repositories: the user has to sign
            // in again to grant it.
            Err(CodeHostError::Forbidden(_)) if self.identity == IdentityProvider::Oauth => {
                return Err(CodeHostError::MissingScope(REPO_SCOPE.into()));
            }
            r => r?,
        };
        let mut ids: Vec<String> = repos
            .into_iter()
            .filter(|r| {
                r.visibility() != RepoVisibility::Public && r.permissions.is_none_or(|p| p.pull)
            })
            .map(|r| r.id.to_string())
            .collect();
        ids.sort();
        ids.dedup();
        Ok(UserPerms { external_ids: ids })
    }

    async fn fetch_repo_perms(&self, external_id: &str) -> Result<Vec<ExternalAccountData>> {
        if !self.supports_repo_perms() {
            return Err(CodeHostError::Unsupported(
                "repo-centric sync needs identityProvider \"username\"".into(),
            ));
        }
        let repo = self.client.repo_by_id(external_id).await?;
        let path = repo_path(&repo.full_name);
        let mut users: Vec<UserRef> = self
            .client
            .paginate(
                self.client
                    .api_url(&format!("repos/{path}/collaborators"))?,
            )
            .try_collect()
            .await?;
        let owner = path.split('/').next().unwrap_or_default();
        let org = match self.client.get::<Value>(&format!("orgs/{owner}")).await {
            Ok(_) => true,
            Err(CodeHostError::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        if org {
            // Not paginated: Gitea answers every team at once. An organization's owners are
            // its `Owners` team.
            let teams: Vec<Team> = self.client.get(&format!("repos/{path}/teams")).await?;
            for team in teams.iter().filter(|t| t.reads_code()) {
                let url = self.client.api_url(&format!("teams/{}/members", team.id))?;
                let members: Vec<UserRef> = self.client.paginate(url).try_collect().await?;
                users.extend(members);
            }
        } else {
            // A user's repository is readable by its owner.
            users.push(self.client.get(&format!("users/{owner}")).await?);
        }
        let mut accounts: Vec<ExternalAccountData> = users
            .into_iter()
            .filter(UserRef::usable)
            .map(UserRef::account)
            .collect();
        // A user can be a collaborator and on several teams.
        accounts.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        accounts.dedup_by(|a, b| a.account_id == b.account_id);
        Ok(accounts)
    }

    async fn fetch_account(
        &self,
        username: &str,
        _verified_emails: &[String],
    ) -> Result<Option<ExternalAccountData>> {
        if self.identity != IdentityProvider::Username {
            // OAuth accounts come from signing in.
            return Ok(None);
        }
        let q: String = url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
        let user: UserRef = match self.client.get(&format!("users/{q}")).await {
            Ok(u) => u,
            Err(CodeHostError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok((user.login.eq_ignore_ascii_case(username) && user.usable()).then(|| user.account()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(permission: &str, units: Option<Value>) -> Team {
        Team {
            id: 1,
            permission: Some(permission.into()),
            units_map: units,
        }
    }

    #[test]
    fn team_code_access() {
        assert!(team("read", None).reads_code());
        assert!(team("owner", None).reads_code());
        assert!(!team("none", None).reads_code());
        assert!(team("none", Some(serde_json::json!({"repo.code": "read"}))).reads_code());
        assert!(!team("write", Some(serde_json::json!({"repo.code": "none"}))).reads_code());
        assert!(team("admin", Some(serde_json::json!({"repo.code": "none"}))).reads_code());
    }
}
