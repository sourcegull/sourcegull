//! The connection pool wrapper.

use crate::Result;
use sourcegull_crypto::Keyring;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// Pool and session settings for [`Db::connect`].
#[derive(Debug, Clone)]
pub struct DbOptions {
    /// Maximum pool size.
    pub max_connections: u32,
    /// Connections kept open when idle.
    pub min_connections: u32,
    /// How long to wait for a free connection.
    pub acquire_timeout: Duration,
    /// Close connections idle for longer than this.
    pub idle_timeout: Option<Duration>,
    /// Server-side `statement_timeout` for every connection; `None` keeps the
    /// server default.
    pub statement_timeout: Option<Duration>,
    /// `application_name` reported to Postgres (visible in `pg_stat_activity`).
    pub application_name: String,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            max_connections: 20,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(10),
            idle_timeout: Some(Duration::from_secs(600)),
            statement_timeout: Some(Duration::from_secs(60)),
            application_name: "sourcegull".to_owned(),
        }
    }
}

/// Handle to the main database. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connects to `url` (a `postgres://` URL) with the given options.
    pub async fn connect(url: &str, opts: &DbOptions) -> Result<Self> {
        let mut connect = PgConnectOptions::from_str(url)?.application_name(&opts.application_name);
        if let Some(timeout) = opts.statement_timeout {
            connect = connect.options([("statement_timeout", timeout.as_millis().to_string())]);
        }
        let pool = PgPoolOptions::new()
            .max_connections(opts.max_connections)
            .min_connections(opts.min_connections)
            .acquire_timeout(opts.acquire_timeout)
            .idle_timeout(opts.idle_timeout)
            .connect_with(connect)
            .await?;
        Ok(Self { pool })
    }

    /// A second pool on the same database with its own size and `application_name` (e.g. the
    /// worker role's budget, so background jobs cannot starve requests). Connects lazily; the
    /// server, credentials and session settings (`statement_timeout`) are this pool's.
    pub fn sibling(&self, opts: &DbOptions) -> Self {
        let connect = (*self.pool.connect_options())
            .clone()
            .application_name(&opts.application_name);
        let pool = PgPoolOptions::new()
            .max_connections(opts.max_connections)
            .min_connections(opts.min_connections)
            .acquire_timeout(opts.acquire_timeout)
            .idle_timeout(opts.idle_timeout)
            .connect_lazy_with(connect);
        Self { pool }
    }

    /// Wraps an existing pool (useful in tests).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Store for `users`.
    pub fn users(&self) -> crate::UserStore {
        crate::UserStore::new(self.pool.clone())
    }

    /// Store for SCIM-controlled users.
    pub fn scim(&self) -> crate::ScimStore {
        crate::ScimStore::new(self.pool.clone())
    }

    /// Store for `user_emails`.
    pub fn user_emails(&self) -> crate::UserEmailStore {
        crate::UserEmailStore::new(self.pool.clone())
    }

    /// Store for single-use password reset codes.
    pub fn password_resets(&self) -> crate::PasswordResetStore {
        crate::PasswordResetStore::new(self.pool.clone())
    }

    /// Store for `orgs` and `org_members`.
    pub fn orgs(&self) -> crate::OrgStore {
        crate::OrgStore::new(self.pool.clone())
    }

    /// Store for `site_config`.
    pub fn site_config(&self) -> crate::SiteConfigStore {
        crate::SiteConfigStore::new(self.pool.clone())
    }

    /// Store for `settings`.
    pub fn settings(&self) -> crate::SettingsStore {
        crate::SettingsStore::new(self.pool.clone())
    }

    /// Store for `temporary_settings` (per-user UI state such as recent searches).
    pub fn temporary_settings(&self) -> crate::TemporarySettingsStore {
        crate::TemporarySettingsStore::new(self.pool.clone())
    }

    /// Store for `event_logs` (usage events).
    pub fn event_logs(&self) -> crate::EventLogStore {
        crate::EventLogStore::new(self.pool.clone())
    }

    /// Store for `security_event_logs`.
    pub fn security_events(&self) -> crate::SecurityEventStore {
        crate::SecurityEventStore::new(self.pool.clone())
    }

    /// Atomic sign-up account creation (`users`, `user_roles`, `user_emails`).
    pub fn accounts(&self) -> crate::AccountStore {
        crate::AccountStore::new(self.pool.clone())
    }

    /// Store for `access_tokens`.
    pub fn access_tokens(&self) -> crate::AccessTokenStore {
        crate::AccessTokenStore::new(self.pool.clone())
    }

    /// Store for the OAuth server's clients, authorization codes, grants and tokens.
    pub fn oauth(&self) -> crate::OAuthStore {
        crate::OAuthStore::new(self.pool.clone())
    }

    /// Store for `sessions`.
    pub fn sessions(&self) -> crate::SessionRowStore {
        crate::SessionRowStore::new(self.pool.clone())
    }

    /// Store for the configuration-as-code tables (`declared_*`).
    pub fn declared(&self) -> crate::DeclaredStore {
        crate::DeclaredStore::new(self.pool.clone())
    }

    /// Store for `roles`, `permissions`, `role_permissions` and `user_roles`.
    pub fn rbac(&self) -> crate::RbacStore {
        crate::RbacStore::new(self.pool.clone())
    }

    /// Store for `encryption_keys` (data keys).
    pub fn encryption_keys(&self) -> crate::EncryptionKeyStore {
        crate::EncryptionKeyStore::new(self.pool.clone())
    }

    /// Store for `out_of_band_migrations`.
    pub fn oob_migrations(&self) -> crate::OutOfBandMigrationStore {
        crate::OutOfBandMigrationStore::new(self.pool.clone())
    }

    /// Store for `external_services`. Configuration reads and writes need `keyring`.
    pub fn external_services(&self, keyring: Option<Arc<Keyring>>) -> crate::ExternalServiceStore {
        crate::ExternalServiceStore::new(self.pool.clone(), keyring)
    }

    /// Store for `user_external_accounts` and `oauth_flows`. Token reads and writes need
    /// `keyring`.
    pub fn external_accounts(&self, keyring: Option<Arc<Keyring>>) -> crate::ExternalAccountStore {
        crate::ExternalAccountStore::new(self.pool.clone(), keyring)
    }

    /// Store for `external_service_sync_jobs`.
    pub fn sync_jobs(&self) -> crate::SyncJobStore {
        crate::SyncJobStore::new(self.pool.clone())
    }

    /// Store for `repo`, `external_service_repos` and `repo_kvps`.
    pub fn perms(&self) -> crate::PermsStore {
        crate::PermsStore::new(self.pool.clone())
    }

    /// Store for `access_requests` and `access_request_invites`.
    pub fn access_requests(&self) -> crate::AccessRequestStore {
        crate::AccessRequestStore::new(self.pool.clone())
    }

    /// Store for `permission_sync_jobs` and `user_permissions_sync`.
    pub fn perms_sync_jobs(&self) -> crate::PermsSyncJobStore {
        crate::PermsSyncJobStore::new(self.pool.clone())
    }

    /// The `repo` store (internal: unfiltered by permissions; viewers go through the
    /// `*_authorized` methods).
    pub fn repos(&self) -> crate::RepoStore {
        crate::RepoStore::new(self.pool.clone())
    }

    /// Store for `gitserver_repos`.
    pub fn gitserver_repos(&self) -> crate::GitserverRepoStore {
        crate::GitserverRepoStore::new(self.pool.clone())
    }

    /// Store for `search_index_repos` and the repository admin aggregates.
    pub fn search_index(&self) -> crate::SearchIndexStore {
        crate::SearchIndexStore::new(self.pool.clone())
    }

    /// Store for `repo_nodes` and `repo_moves` (rebalancing).
    pub fn repo_moves(&self) -> crate::RepoMoveStore {
        crate::RepoMoveStore::new(self.pool.clone())
    }

    /// Store for `repo_update_jobs`.
    pub fn repo_update_jobs(&self) -> crate::RepoUpdateJobStore {
        crate::RepoUpdateJobStore::new(self.pool.clone())
    }

    /// Store for `repo_ref_events`.
    pub fn ref_events(&self) -> crate::RefEventStore {
        crate::RefEventStore::new(self.pool.clone())
    }

    /// Store for `outbound_requests`, the outbound request log.
    pub fn outbound_requests(&self) -> crate::OutboundRequestStore {
        crate::OutboundRequestStore::new(self.pool.clone())
    }

    /// Store for `slow_requests`, the slow request log.
    pub fn slow_requests(&self) -> crate::SlowRequestStore {
        crate::SlowRequestStore::new(self.pool.clone())
    }

    /// Store for `feature_flags` and their overrides.
    pub fn feature_flags(&self) -> crate::FeatureFlagStore {
        crate::FeatureFlagStore::new(self.pool.clone())
    }

    /// Store for `webhook_logs`.
    pub fn webhook_logs(&self) -> crate::WebhookLogStore {
        crate::WebhookLogStore::new(self.pool.clone())
    }

    /// Store for `search_contexts` and their repos, stars and defaults.
    pub fn search_contexts(&self) -> crate::SearchContextStore {
        crate::SearchContextStore::new(self.pool.clone())
    }

    /// Store for `executor_heartbeats` and `executor_job_tokens`.
    pub fn executors(&self) -> crate::ExecutorStore {
        crate::ExecutorStore::new(self.pool.clone())
    }

    /// Store for `executor_secrets` and `executor_secret_access_logs`. Value reads and
    /// writes need `keyring`.
    pub fn executor_secrets(&self, keyring: Option<Arc<Keyring>>) -> crate::ExecutorSecretStore {
        crate::ExecutorSecretStore::new(self.pool.clone(), keyring)
    }

    /// Store for `saved_searches`.
    pub fn saved_searches(&self) -> crate::SavedSearchStore {
        crate::SavedSearchStore::new(self.pool.clone())
    }
}
