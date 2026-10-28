//! Repository permissions from GitLab, implementing [`AuthzProvider`].
//!
//! - `identityProvider: oauth` (user-centric only): lists the projects the user can read with
//!   their own OAuth token.
//! - `identityProvider: username`: finds the GitLab user with the Sourcegull username (`GET
//!   /users?username=`), and lists their projects with the connection's admin token plus
//!   `Sudo: <user id>`. Also repo-centric: `GET /projects/:id/members/all` (direct and inherited
//!   members).
//!
//! "Can read" is Reporter or above (`min_access_level=20`), the lowest role that may read code.
//! Only private projects are listed, plus internal ones unless `markInternalReposAsPublic`
//! (public projects, and internal ones marked public, are readable by everyone anyway).

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use serde::Deserialize;
use sourcegull_codehost::{AuthzProvider, CodeHostError, ExternalAccountData, Result, UserPerms};

use crate::client::{Auth, GitLabClient, PER_PAGE};
use crate::config::{GitLabConnection, IdentityProvider};
use crate::source::SERVICE_TYPE;

/// The lowest access level that can read code (Reporter).
pub const READ_ACCESS_LEVEL: i64 = 20;

/// The permissions provider of one connection with an `authorization` block.
#[derive(Debug, Clone)]
pub struct GitLabAuthzProvider {
    service_id: String,
    client: Arc<GitLabClient>,
    identity: IdentityProvider,
    internal_public: bool,
}

#[derive(Deserialize)]
struct ProjectId {
    id: i64,
}

#[derive(Deserialize)]
struct Member {
    id: i64,
    username: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    access_level: i64,
}

#[derive(Deserialize)]
struct UserRef {
    id: i64,
    username: String,
    #[serde(default)]
    state: Option<String>,
}

fn active(state: Option<&str>) -> bool {
    state.is_none_or(|s| s == "active")
}

impl GitLabAuthzProvider {
    /// The provider of `conn`, or `None` when it has no `authorization` block. `client` is
    /// the connection's API client (its token and rate limiter).
    pub fn new(conn: &GitLabConnection, client: Arc<GitLabClient>) -> Result<Option<Self>> {
        let Some(authz) = &conn.authorization else {
            return Ok(None);
        };
        let service_id = conn.base_url().map_err(CodeHostError::Config)?.to_string();
        Ok(Some(Self {
            service_id,
            client,
            identity: authz.identity_provider.clone(),
            internal_public: conn.mark_internal_repos_as_public,
        }))
    }

    /// The visibilities whose projects need permissions.
    fn visibilities(&self) -> &'static [&'static str] {
        if self.internal_public {
            &["private"]
        } else {
            &["private", "internal"]
        }
    }
}

#[async_trait]
impl AuthzProvider for GitLabAuthzProvider {
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
                    .clone()
                    .ok_or(CodeHostError::Unauthorized)?;
                self.client.with_auth(Auth::Bearer(token))
            }
            IdentityProvider::Username => self.client.with_sudo(&account.account_id),
        };
        let mut ids = Vec::new();
        for visibility in self.visibilities() {
            let first = client.api_url(&format!(
                "projects?min_access_level={READ_ACCESS_LEVEL}&visibility={visibility}\
                 &simple=true&per_page={PER_PAGE}&pagination=keyset&order_by=id&sort=asc"
            ))?;
            let page: Vec<ProjectId> = client.paginate(first).try_collect().await?;
            ids.extend(page.into_iter().map(|p| p.id.to_string()));
        }
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
        let id: String = url::form_urlencoded::byte_serialize(external_id.as_bytes()).collect();
        let first = self
            .client
            .api_url(&format!("projects/{id}/members/all?per_page={PER_PAGE}"))?;
        let members: Vec<Member> = self.client.paginate(first).try_collect().await?;
        let mut accounts: Vec<ExternalAccountData> = members
            .into_iter()
            .filter(|m| m.access_level >= READ_ACCESS_LEVEL && active(m.state.as_deref()))
            .map(|m| ExternalAccountData {
                account_id: m.id.to_string(),
                username: m.username,
                access_token: None,
                expires_at: None,
            })
            .collect();
        // Inherited and direct memberships can both be listed.
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
        let users: Vec<UserRef> = self.client.get(&format!("users?username={q}")).await?;
        Ok(users
            .into_iter()
            .find(|u| u.username.eq_ignore_ascii_case(username) && active(u.state.as_deref()))
            .map(|u| ExternalAccountData {
                account_id: u.id.to_string(),
                username: u.username,
                access_token: None,
                expires_at: None,
            }))
    }
}
