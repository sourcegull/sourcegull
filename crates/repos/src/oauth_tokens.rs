//! OAuth tokens of GitLab connections, refreshed and stored back into the connection's
//! configuration.
//!
//! [`ConnectionTokens`] keeps one [`TokenSession`] per connection in the process, so every API
//! client of the connection shares its tokens and refreshes run one at a time. A refresh:
//!
//! 1. locks the connection's row and reads the stored configuration;
//! 2. uses the stored tokens when another process has already refreshed them;
//! 3. otherwise exchanges the stored refresh token at `POST {url}/oauth/token` with the OAuth
//!    application of the `auth.providers` GitLab entry for the same URL (each matching entry
//!    is tried until GitLab accepts one);
//! 4. writes only the token fields back (`token`, `token.oauth.refresh`,
//!    `token.oauth.expiry`), keeping the sync state, and releases the lock.
//!
//! An admin's configuration update waits for the lock, and new tokens it sets replace the
//! session's ([`TokenSession::observe`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

use async_trait::async_trait;
use sourcegull_codehost::{CodeHostError, OAuthLoginProvider, Result};
use sourcegull_codehost_gitlab::config::{normalize_base_url, set_oauth_tokens};
use sourcegull_codehost_gitlab::token::REFRESH_MARGIN;
use sourcegull_codehost_gitlab::{
    GitLabConnection, GitLabOAuth, OAuthTokens, TokenRefresher, TokenSession,
};
use sourcegull_config::{AuthProvider, SiteConfigHandle};
use sourcegull_db::{DbError, ExternalServiceStore};
use sourcegull_types::ExternalServiceId;

/// The token sessions of a process's GitLab OAuth connections.
pub struct ConnectionTokens {
    store: ExternalServiceStore,
    config: SiteConfigHandle,
    sessions: Mutex<HashMap<i64, Arc<TokenSession>>>,
}

impl std::fmt::Debug for ConnectionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionTokens").finish_non_exhaustive()
    }
}

impl ConnectionTokens {
    /// Sessions storing refreshed tokens through `store` and refreshing with the OAuth
    /// applications of `config`'s `auth.providers`.
    pub fn new(store: ExternalServiceStore, config: SiteConfigHandle) -> Self {
        Self {
            store,
            config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The token session of connection `id`, whose configuration is `conn`; `None` unless it
    /// uses an OAuth token.
    pub fn session(&self, id: i64, conn: &GitLabConnection) -> Option<Arc<TokenSession>> {
        let tokens = OAuthTokens::from_connection(conn)?;
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(s) = sessions.get(&id) {
            s.observe(&tokens);
            return Some(s.clone());
        }
        let refresher = Arc::new(DbRefresher {
            id: ExternalServiceId::from(id),
            store: self.store.clone(),
            config: self.config.clone(),
        });
        let s = Arc::new(TokenSession::new(tokens, refresher));
        sessions.insert(id, s.clone());
        Some(s)
    }
}

fn global_slot() -> &'static RwLock<Option<Arc<ConnectionTokens>>> {
    static GLOBAL: OnceLock<RwLock<Option<Arc<ConnectionTokens>>>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(None))
}

/// The process-wide token sessions, if [`set_global`] installed them.
pub fn global() -> Option<Arc<ConnectionTokens>> {
    global_slot()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Installs the process-wide token sessions (at startup, before clients are built).
pub fn set_global(tokens: Arc<ConnectionTokens>) {
    *global_slot()
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Some(tokens);
}

#[derive(Debug)]
struct DbRefresher {
    id: ExternalServiceId,
    store: ExternalServiceStore,
    config: SiteConfigHandle,
}

fn db_error(e: DbError) -> CodeHostError {
    CodeHostError::Transport(format!("reading the connection: {e}"))
}

impl DbRefresher {
    /// The OAuth applications configured for the GitLab instance at `url`.
    fn apps(&self, conn: &GitLabConnection) -> Result<Vec<GitLabOAuth>> {
        let base = conn.base_url().map_err(CodeHostError::Config)?;
        let config = self.config.load();
        let apps: Vec<GitLabOAuth> = config
            .value
            .auth_providers
            .iter()
            .filter_map(|p| match p {
                AuthProvider::Gitlab(g) if normalize_base_url(&g.url).is_ok_and(|u| u == base) => {
                    GitLabOAuth::for_refresh(
                        &g.url,
                        &g.client_id,
                        &g.client_secret,
                        conn.certificate.as_deref(),
                    )
                    .ok()
                }
                _ => None,
            })
            .collect();
        if apps.is_empty() {
            return Err(CodeHostError::Config(format!(
                "the OAuth token cannot be refreshed: auth.providers has no GitLab entry for \
                 {base} (add one with the OAuth application that issued the token)"
            )));
        }
        Ok(apps)
    }
}

#[async_trait]
impl TokenRefresher for DbRefresher {
    async fn refresh(&self, stale: &OAuthTokens) -> Result<OAuthTokens> {
        let locked = self.store.lock_config(self.id).await.map_err(db_error)?;
        let conn = sourcegull_codehost_gitlab::validate_connection(&locked.config)
            .map_err(|e| CodeHostError::Config(e.to_string()))?
            .config;
        let stored = OAuthTokens::from_connection(&conn)
            .ok_or_else(|| CodeHostError::Config("the connection no longer uses OAuth".into()))?;
        let now = chrono::Utc::now().timestamp();
        if stored.access_token != stale.access_token && !stored.expires_within(now, REFRESH_MARGIN)
        {
            // Refreshed by another process (or replaced by an admin) since `stale` was read.
            return Ok(stored);
        }
        let Some(refresh_token) = stored.refresh_token.clone() else {
            return Err(CodeHostError::Unauthorized);
        };
        let mut rejected = None;
        let mut token = None;
        for app in self.apps(&conn)? {
            match app.refresh(&refresh_token).await {
                Ok(t) => {
                    token = Some(t);
                    break;
                }
                Err(e @ CodeHostError::Unauthorized) => rejected = Some(e),
                Err(e) => return Err(e),
            }
        }
        let Some(t) = token else {
            return Err(rejected.unwrap_or(CodeHostError::Unauthorized));
        };
        let new = OAuthTokens {
            access_token: t.access_token,
            // GitLab always rotates; keep the old one if a response ever lacks it.
            refresh_token: t.refresh_token.or(Some(refresh_token)),
            expires_at: t.expires_at.map(|at| at.timestamp()),
        };
        let text = set_oauth_tokens(&locked.config, &new)
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        if let Err(e) = locked.replace(&text).await {
            // The old refresh token is spent: keep using the new tokens in this process.
            tracing::error!(connection = %self.id, error = %e, "storing refreshed GitLab OAuth tokens failed");
        } else {
            tracing::info!(connection = %self.id, "refreshed the GitLab OAuth token");
        }
        Ok(new)
    }
}
