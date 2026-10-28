//! Repository permissions from the code host: [`AuthzProvider`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Result;

/// A user's account id on the code host (the GitLab user id, as a string).
pub type AccountId = String;

/// What the permissions sync knows about a user's account on the code host: the identity
/// plus the (decrypted) OAuth token when the user signed in with the code host.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAccountData {
    /// The account id on the code host.
    pub account_id: AccountId,
    /// The username on the code host.
    pub username: String,
    /// The user's OAuth access token, when available (the `oauth` identity provider).
    pub access_token: Option<String>,
    /// When `access_token` expires.
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for ExternalAccountData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccountData")
            .field("account_id", &self.account_id)
            .field("username", &self.username)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The repos a user may read, by external id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPerms {
    /// External ids of readable repos (exact).
    pub external_ids: Vec<String>,
}

/// Fetches repository permissions from a code host.
#[async_trait]
pub trait AuthzProvider: Send + Sync {
    /// The code host instance this provider covers (`ExternalRepoSpec::service_id`).
    fn service_id(&self) -> &str;

    /// The code host type (`ExternalRepoSpec::service_type`, e.g. `gitlab`).
    fn service_type(&self) -> &str;

    /// Whether user-centric syncs need the user's own OAuth token (from signing in with the
    /// code host). Otherwise the provider finds accounts itself ([`fetch_account`]) and reads
    /// with the connection's credentials.
    ///
    /// [`fetch_account`]: AuthzProvider::fetch_account
    fn needs_user_token(&self) -> bool;

    /// Whether [`fetch_repo_perms`](AuthzProvider::fetch_repo_perms) is supported.
    fn supports_repo_perms(&self) -> bool;

    /// The repos `account` can read (user-centric sync).
    async fn fetch_user_perms(&self, account: &ExternalAccountData) -> Result<UserPerms>;

    /// The accounts that can read the repo with external id `external_id` (repo-centric sync),
    /// without tokens. Providers that cannot answer return
    /// [`CodeHostError::Unsupported`](crate::CodeHostError::Unsupported).
    async fn fetch_repo_perms(&self, external_id: &str) -> Result<Vec<ExternalAccountData>>;

    /// Finds the code host account of a Sourcegull user who did not sign in with the code host,
    /// e.g. by username or verified email (the `username` identity provider).
    async fn fetch_account(
        &self,
        username: &str,
        verified_emails: &[String],
    ) -> Result<Option<ExternalAccountData>>;
}
