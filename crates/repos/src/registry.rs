//! The code host registry: maps a connection's [`ExternalServiceKind`] to its implementations.
//! Adding a code host means one new `codehost-*` crate and one arm per function here.

use std::sync::Arc;

use serde_json::Value;
use sourcegull_codehost::{
    AuthzProvider, ChangesetSource, CiJobVerifier, ClientOptions, CodeHostError, GroupSource,
    OAuthLoginProvider, RateLimiter, RateLimiterState, RateLimiters, RepoSource, WebhookHandler,
    WebhookRegistrar,
};
use sourcegull_codehost_gitea::{
    GiteaAuthzProvider, GiteaClient, GiteaConnection, GiteaOAuth, GiteaSource, GiteaWebhooks,
};
use sourcegull_codehost_github::{
    GitHubAuthzProvider, GitHubClient, GitHubConnection, GitHubSource, GitHubWebhooks,
};
use sourcegull_codehost_gitlab::{
    GitLabAuthzProvider, GitLabChangesetSource, GitLabCiJobs, GitLabClient, GitLabConnection,
    GitLabGroups, GitLabHooks, GitLabOAuth, GitLabSource, GitLabWebhooks,
};
use sourcegull_config::{AuthProvider, ConfigError, ConfigIssue};
use sourcegull_types::ExternalServiceKind;

/// A validated connection configuration's non-fatal findings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigCheck {
    /// Warnings to show the admin (e.g. ignored keys).
    pub warnings: Vec<ConfigIssue>,
}

/// Builds code host implementations from connection configurations.
///
/// API clients draw from the connection's shared request budget: the limiter of
/// [`connection_id`](Self::connection_id) (set with [`for_connection`](Self::for_connection))
/// in [`limiters`](Self::limiters). Without a connection id, the budget is keyed by the
/// instance URL and a hash of the token.
///
/// The API clients of an OAuth connection with a known id refresh its token through the
/// connection's [`TokenSession`](sourcegull_codehost_gitlab::TokenSession) from
/// [`oauth_tokens`](Self::oauth_tokens).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// API client tuning for every connection.
    pub client_options: ClientOptions,
    /// Where rate limiters come from; `None` = the process-wide registry
    /// ([`sourcegull_codehost::ratelimit::global`]).
    pub limiters: Option<Arc<dyn RateLimiters>>,
    /// The code host connection (external service id) the configurations belong to.
    pub connection_id: Option<i64>,
    /// Where OAuth token sessions come from; `None` = the process-wide ones
    /// ([`crate::oauth_tokens::global`]). Without either, OAuth tokens are not refreshed.
    pub oauth_tokens: Option<Arc<crate::oauth_tokens::ConnectionTokens>>,
}

/// A webhook registrar and the secret the hooks it registers carry.
pub type SecretRegistrar = (Arc<dyn WebhookRegistrar>, String);

fn gitlab(text: &str) -> Result<GitLabConnection, CodeHostError> {
    sourcegull_codehost_gitlab::validate_connection(text)
        .map(|v| v.config)
        .map_err(|e| CodeHostError::Config(e.to_string()))
}

fn github(text: &str) -> Result<GitHubConnection, CodeHostError> {
    sourcegull_codehost_github::validate_connection(text)
        .map(|v| v.config)
        .map_err(|e| CodeHostError::Config(e.to_string()))
}

fn gitea(text: &str) -> Result<GiteaConnection, CodeHostError> {
    sourcegull_codehost_gitea::validate_connection(text)
        .map(|v| v.config)
        .map_err(|e| CodeHostError::Config(e.to_string()))
}

/// What a code host does not support, as an error.
fn unsupported(kind: ExternalServiceKind, what: &str) -> CodeHostError {
    CodeHostError::Unsupported(format!("{what} are not supported for {kind} connections"))
}

/// The limiter key of a connection without a known id: the host type, the instance URL and a
/// hash of the token, so the token itself never leaves the process (it may be stored in
/// Postgres).
fn token_key(service_type: &str, base: &str, token: &str) -> String {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(token.as_bytes());
    format!("{service_type}:{base}#{}", hex::encode(&hash[..8]))
}

/// The limiter key parts of a connection: the instance URL and the token.
trait LimiterKey {
    fn service_type(&self) -> &'static str;
    fn base(&self) -> String;
    fn token(&self) -> &str;
}

impl LimiterKey for GitLabConnection {
    fn service_type(&self) -> &'static str {
        sourcegull_codehost_gitlab::SERVICE_TYPE
    }
    fn base(&self) -> String {
        self.base_url()
            .map_or_else(|_| self.url.clone(), |u| u.to_string())
    }
    fn token(&self) -> &str {
        &self.token
    }
}

impl LimiterKey for GitHubConnection {
    fn service_type(&self) -> &'static str {
        sourcegull_codehost_github::SERVICE_TYPE
    }
    fn base(&self) -> String {
        self.base_url()
            .map_or_else(|_| self.url.clone(), |u| u.to_string())
    }
    fn token(&self) -> &str {
        self.credential_key()
    }
}

impl LimiterKey for GiteaConnection {
    fn service_type(&self) -> &'static str {
        sourcegull_codehost_gitea::SERVICE_TYPE
    }
    fn base(&self) -> String {
        self.base_url()
            .map_or_else(|_| self.url.clone(), |u| u.to_string())
    }
    fn token(&self) -> &str {
        &self.token
    }
}

impl Registry {
    /// This registry for the configurations of connection `id`: its API clients share the
    /// connection's request budget.
    pub fn for_connection(&self, id: i64) -> Self {
        Self {
            connection_id: Some(id),
            ..self.clone()
        }
    }

    fn rate_limiters(&self) -> Arc<dyn RateLimiters> {
        self.limiters
            .clone()
            .unwrap_or_else(sourcegull_codehost::ratelimit::global)
    }

    fn limiter_key(&self, conn: &impl LimiterKey) -> String {
        self.connection_id.map_or_else(
            || token_key(conn.service_type(), &conn.base(), conn.token()),
            sourcegull_codehost::connection_key,
        )
    }

    fn github_client(&self, conn: &GitHubConnection) -> Result<GitHubClient, CodeHostError> {
        let limiter: Arc<dyn RateLimiter> = self
            .rate_limiters()
            .limiter(&self.limiter_key(conn), conn.requests_per_hour());
        GitHubClient::with_limiter(conn, limiter, self.client_options.clone())
    }

    fn gitea_client(&self, conn: &GiteaConnection) -> Result<GiteaClient, CodeHostError> {
        let limiter: Arc<dyn RateLimiter> = self
            .rate_limiters()
            .limiter(&self.limiter_key(conn), conn.requests_per_hour());
        GiteaClient::with_limiter(conn, limiter, self.client_options.clone())
    }

    fn gitlab_client(&self, conn: &GitLabConnection) -> Result<GitLabClient, CodeHostError> {
        let limiter: Arc<dyn RateLimiter> = self
            .rate_limiters()
            .limiter(&self.limiter_key(conn), conn.requests_per_hour());
        let client = GitLabClient::with_limiter(conn, limiter, self.client_options.clone())?;
        let session = self.connection_id.and_then(|id| {
            self.oauth_tokens
                .clone()
                .or_else(crate::oauth_tokens::global)?
                .session(id, conn)
        });
        Ok(match session {
            Some(s) => client.with_token_session(s),
            None => client,
        })
    }

    /// The current state of a configuration's request budget (`ExternalService.rateLimiterState`).
    pub async fn rate_limiter_state(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<RateLimiterState, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                self.rate_limiters()
                    .state(&self.limiter_key(&conn), conn.requests_per_hour())
                    .await
            }
            ExternalServiceKind::GitHub => {
                let conn = github(text)?;
                self.rate_limiters()
                    .state(&self.limiter_key(&conn), conn.requests_per_hour())
                    .await
            }
            ExternalServiceKind::Gitea => {
                let conn = gitea(text)?;
                self.rate_limiters()
                    .state(&self.limiter_key(&conn), conn.requests_per_hour())
                    .await
            }
        }
    }

    /// Validates a configuration of `kind` (schema and semantic rules).
    pub fn validate(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<ConfigCheck, ConfigError> {
        match kind {
            ExternalServiceKind::GitLab => sourcegull_codehost_gitlab::validate_connection(text)
                .map(|v| ConfigCheck {
                    warnings: v.warnings,
                }),
            ExternalServiceKind::GitHub => sourcegull_codehost_github::validate_connection(text)
                .map(|v| ConfigCheck {
                    warnings: v.warnings,
                }),
            ExternalServiceKind::Gitea => {
                sourcegull_codehost_gitea::validate_connection(text).map(|v| ConfigCheck {
                    warnings: v.warnings,
                })
            }
        }
    }

    /// Whether a configuration leaves the private repos it lists readable by every user: it
    /// has no `authorization` block. An unparsable configuration is restricted.
    pub fn unrestricted(&self, kind: ExternalServiceKind, text: &str) -> bool {
        match kind {
            ExternalServiceKind::GitLab => gitlab(text).is_ok_and(|c| c.authorization.is_none()),
            ExternalServiceKind::GitHub => github(text).is_ok_and(|c| c.authorization.is_none()),
            ExternalServiceKind::Gitea => gitea(text).is_ok_and(|c| c.authorization.is_none()),
        }
    }

    /// Whether a configuration's permissions are also synced per repository (repo-centric:
    /// GitLab and Gitea `identityProvider: username`, GitHub with an `authorization` block), so
    /// new repositories get a sync of their own.
    pub fn repo_centric_perms(&self, kind: ExternalServiceKind, text: &str) -> bool {
        match kind {
            ExternalServiceKind::GitLab => gitlab(text).is_ok_and(|c| {
                c.authorization.is_some_and(|a| {
                    a.identity_provider
                        == sourcegull_codehost_gitlab::config::IdentityProvider::Username
                })
            }),
            ExternalServiceKind::Gitea => gitea(text).is_ok_and(|c| {
                c.authorization.is_some_and(|a| {
                    a.identity_provider
                        == sourcegull_codehost_gitea::config::IdentityProvider::Username
                })
            }),
            ExternalServiceKind::GitHub => github(text).is_ok_and(|c| c.authorization.is_some()),
        }
    }

    /// The JSON Schema of `kind`'s configuration.
    pub fn schema(&self, kind: ExternalServiceKind) -> Value {
        match kind {
            ExternalServiceKind::GitLab => sourcegull_codehost_gitlab::connection_schema(),
            ExternalServiceKind::GitHub => sourcegull_codehost_github::connection_schema(),
            ExternalServiceKind::Gitea => sourcegull_codehost_gitea::connection_schema(),
        }
    }

    /// The configuration with secrets replaced by `REDACTED`.
    pub fn redact(&self, kind: ExternalServiceKind, text: &str) -> Result<String, ConfigError> {
        match kind {
            ExternalServiceKind::GitLab => sourcegull_codehost_gitlab::config::redact(text),
            ExternalServiceKind::GitHub => sourcegull_codehost_github::config::redact(text),
            ExternalServiceKind::Gitea => sourcegull_codehost_gitea::config::redact(text),
        }
    }

    /// `new` with `REDACTED` placeholders filled in from `old`.
    pub fn unredact(
        &self,
        kind: ExternalServiceKind,
        new: &str,
        old: &str,
    ) -> Result<String, ConfigError> {
        match kind {
            ExternalServiceKind::GitLab => sourcegull_codehost_gitlab::config::unredact(new, old),
            ExternalServiceKind::GitHub => sourcegull_codehost_github::config::unredact(new, old),
            ExternalServiceKind::Gitea => sourcegull_codehost_gitea::config::unredact(new, old),
        }
    }

    /// The webhook secret to configure on the code host, if the configuration has one.
    pub fn webhook_secret(&self, kind: ExternalServiceKind, text: &str) -> Option<String> {
        match kind {
            ExternalServiceKind::GitLab
            | ExternalServiceKind::GitHub
            | ExternalServiceKind::Gitea => sourcegull_config::secrets::webhook_secret(text),
        }
    }

    /// The configuration with its webhook secrets replaced by `secret`.
    pub fn set_webhook_secret(
        &self,
        kind: ExternalServiceKind,
        text: &str,
        secret: &str,
    ) -> Result<String, ConfigError> {
        match kind {
            ExternalServiceKind::GitLab
            | ExternalServiceKind::GitHub
            | ExternalServiceKind::Gitea => {
                sourcegull_config::secrets::set_webhook_secret(text, secret)
            }
        }
    }

    /// Whether `kind`'s configuration supports excluding repositories (`exclude`).
    pub fn supports_repo_exclusion(&self, kind: ExternalServiceKind) -> bool {
        match kind {
            ExternalServiceKind::GitLab
            | ExternalServiceKind::GitHub
            | ExternalServiceKind::Gitea => true,
        }
    }

    /// The code host instance id (`ExternalRepoSpec::service_id`) of a configuration.
    pub fn service_id(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<String, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => gitlab(text)?
                .base_url()
                .map(|u| u.to_string())
                .map_err(CodeHostError::Config),
            ExternalServiceKind::GitHub => github(text)?
                .base_url()
                .map(|u| u.to_string())
                .map_err(CodeHostError::Config),
            ExternalServiceKind::Gitea => gitea(text)?
                .base_url()
                .map(|u| u.to_string())
                .map_err(CodeHostError::Config),
        }
    }

    /// The repo source of a configuration.
    pub fn source(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Arc<dyn RepoSource>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                let client = Arc::new(self.gitlab_client(&conn)?);
                Ok(Arc::new(GitLabSource::new(conn, client)?))
            }
            ExternalServiceKind::GitHub => {
                let conn = github(text)?;
                let client = Arc::new(self.github_client(&conn)?);
                Ok(Arc::new(GitHubSource::new(conn, client)?))
            }
            ExternalServiceKind::Gitea => {
                let conn = gitea(text)?;
                let client = Arc::new(self.gitea_client(&conn)?);
                Ok(Arc::new(GiteaSource::new(conn, client)?))
            }
        }
    }

    /// The changeset source (merge requests for bulk changes) of a configuration,
    /// acting with the connection's token. Each call makes a new API client.
    pub fn changeset_source(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Arc<dyn ChangesetSource>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                let client = Arc::new(self.gitlab_client(&conn)?);
                let source = Arc::new(GitLabSource::new(conn, client)?);
                Ok(Arc::new(GitLabChangesetSource::new(source)))
            }
            ExternalServiceKind::GitHub | ExternalServiceKind::Gitea => {
                Err(unsupported(kind, "bulk changes"))
            }
        }
    }

    /// Whether a configuration receives webhooks from its code host (a webhook secret or the
    /// system hook is configured). An unparsable configuration has none.
    pub fn has_webhooks(&self, kind: ExternalServiceKind, text: &str) -> bool {
        match kind {
            ExternalServiceKind::GitLab => {
                gitlab(text).is_ok_and(|c| !c.webhooks.is_empty() || c.system_hook)
            }
            ExternalServiceKind::GitHub => github(text).is_ok_and(|c| !c.webhooks.is_empty()),
            ExternalServiceKind::Gitea => gitea(text).is_ok_and(|c| !c.webhooks.is_empty()),
        }
    }

    /// Whether a configuration registers its webhooks on the code host
    /// (`webhooks.autoRegister` with a webhook secret). An unparsable configuration does not.
    pub fn registers_webhooks(&self, kind: ExternalServiceKind, text: &str) -> bool {
        match kind {
            ExternalServiceKind::GitLab => gitlab(text).is_ok_and(|c| {
                c.webhooks_auto_register && c.webhook_secrets().any(|s| !s.is_empty())
            }),
            ExternalServiceKind::GitHub | ExternalServiceKind::Gitea => false,
        }
    }

    /// The webhook registrar of a configuration and the secret its hooks carry, or `None`
    /// when it does not [register webhooks](Self::registers_webhooks). Its API client draws
    /// from the connection's request budget.
    pub fn webhook_registrar(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Option<SecretRegistrar>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                let secret = conn
                    .webhook_secrets()
                    .find(|s| !s.is_empty())
                    .map(str::to_owned);
                let Some(secret) = secret.filter(|_| conn.webhooks_auto_register) else {
                    return Ok(None);
                };
                let client = Arc::new(self.gitlab_client(&conn)?);
                Ok(Some((
                    Arc::new(GitLabHooks::new(client)) as Arc<dyn WebhookRegistrar>,
                    secret,
                )))
            }
            ExternalServiceKind::GitHub | ExternalServiceKind::Gitea => Ok(None),
        }
    }

    /// The permissions provider of a configuration, or `None` when it has no `authorization`
    /// block. Each call makes a new API client; callers keep the provider to share
    /// its rate limiter.
    pub fn authz_provider(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Option<Arc<dyn AuthzProvider>>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                if conn.authorization.is_none() {
                    return Ok(None);
                }
                let client = Arc::new(self.gitlab_client(&conn)?);
                Ok(GitLabAuthzProvider::new(&conn, client)?
                    .map(|p| Arc::new(p) as Arc<dyn AuthzProvider>))
            }
            ExternalServiceKind::Gitea => {
                let conn = gitea(text)?;
                if conn.authorization.is_none() {
                    return Ok(None);
                }
                let client = Arc::new(self.gitea_client(&conn)?);
                Ok(GiteaAuthzProvider::new(&conn, client)?
                    .map(|p| Arc::new(p) as Arc<dyn AuthzProvider>))
            }
            ExternalServiceKind::GitHub => {
                let conn = github(text)?;
                if conn.authorization.is_none() {
                    return Ok(None);
                }
                let client = Arc::new(self.github_client(&conn)?);
                Ok(GitHubAuthzProvider::new(&conn, client)?
                    .map(|p| Arc::new(p) as Arc<dyn AuthzProvider>))
            }
        }
    }

    /// The group source of a configuration (teams synced from code host groups).
    /// Each call makes a new API client.
    pub fn group_source(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Arc<dyn GroupSource>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                let client = Arc::new(self.gitlab_client(&conn)?);
                Ok(Arc::new(GitLabGroups::new(&conn, client)?))
            }
            ExternalServiceKind::GitHub | ExternalServiceKind::Gitea => {
                Err(unsupported(kind, "team syncs"))
            }
        }
    }

    /// The CI job token verifier of a configuration's instance. Each call makes a
    /// new API client.
    pub fn ci_job_verifier(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Arc<dyn CiJobVerifier>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => {
                let conn = gitlab(text)?;
                let client = Arc::new(self.gitlab_client(&conn)?);
                Ok(Arc::new(GitLabCiJobs::new(client)))
            }
            ExternalServiceKind::GitHub | ExternalServiceKind::Gitea => {
                Err(unsupported(kind, "CI job tokens"))
            }
        }
    }

    /// The sign-in OAuth application among `providers` that issued an account's token on a
    /// `kind` code host: the entry with `client_id` whose instance URL is `service_id`,
    /// redirecting back to `external_url`. `None` when there is none or `kind`'s tokens are
    /// not refreshed.
    pub fn account_oauth_app(
        &self,
        kind: ExternalServiceKind,
        providers: &[AuthProvider],
        service_id: &str,
        client_id: &str,
        external_url: &str,
    ) -> Option<Arc<dyn OAuthLoginProvider>> {
        match kind {
            ExternalServiceKind::GitLab => providers.iter().find_map(|p| match p {
                AuthProvider::Gitlab(g)
                    if g.client_id == client_id
                        && sourcegull_codehost_gitlab::config::normalize_base_url(&g.url)
                            .is_ok_and(|u| u.as_str() == service_id) =>
                {
                    GitLabOAuth::new(&g.url, &g.client_id, &g.client_secret, external_url)
                        .ok()
                        .map(|app| Arc::new(app) as Arc<dyn OAuthLoginProvider>)
                }
                _ => None,
            }),
            ExternalServiceKind::Gitea => providers.iter().find_map(|p| match p {
                AuthProvider::Gitea(g)
                    if g.client_id == client_id
                        && sourcegull_codehost_gitea::config::normalize_base_url(&g.url)
                            .is_ok_and(|u| u.as_str() == service_id) =>
                {
                    GiteaOAuth::new(&g.url, &g.client_id, &g.client_secret, external_url)
                        .ok()
                        .map(|app| Arc::new(app) as Arc<dyn OAuthLoginProvider>)
                }
                _ => None,
            }),
            ExternalServiceKind::GitHub => None,
        }
    }

    /// The webhook handler of a configuration.
    pub fn webhook_handler(
        &self,
        kind: ExternalServiceKind,
        text: &str,
    ) -> Result<Box<dyn WebhookHandler>, CodeHostError> {
        match kind {
            ExternalServiceKind::GitLab => Ok(Box::new(GitLabWebhooks::new(&gitlab(text)?))),
            ExternalServiceKind::GitHub => Ok(Box::new(GitHubWebhooks::new(&github(text)?))),
            ExternalServiceKind::Gitea => Ok(Box::new(GiteaWebhooks::new(&gitea(text)?))),
        }
    }
}
