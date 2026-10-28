//! OAuth connection tokens that refresh themselves.
//!
//! A connection with `"token.type": "oauth"` holds a GitLab OAuth access token (valid two hours),
//! its refresh token and its expiry. A [`TokenSession`] holds the connection's current tokens
//! and refreshes them through a [`TokenRefresher`]:
//!
//! - before a request, when the access token expires within [`REFRESH_MARGIN`];
//! - after a 401, once, then the request is retried with the new token.
//!
//! Refreshes of one session are serialized: GitLab rotates the refresh token on use, so a
//! second refresh with the old one would fail. A request that was rejected with a token another
//! task has already replaced is retried with the replacement without refreshing again.
//!
//! When GitLab rejects the refresh token (revoked, expired or already used), the session fails
//! every later request with a [`CodeHostError::Config`] that tells the admin to set a new token,
//! without contacting GitLab again, until the connection's configuration brings new tokens
//! ([`TokenSession::observe`]).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use sourcegull_codehost::{CodeHostError, Result};

use crate::config::{GitLabConnection, TokenType};

/// Seconds before expiry at which an access token is refreshed ahead of use.
pub const REFRESH_MARGIN: i64 = 300;

/// How many replaced access tokens a session remembers, to tell a stale configuration (built
/// before the session's last refresh was stored) from a new token set by an admin.
const SEEN_TOKENS: usize = 16;

/// The error of a connection whose refresh token GitLab rejected.
pub const REFRESH_REJECTED: &str = "GitLab rejected the OAuth refresh token (revoked, expired or \
     already used): set a new \"token\" and \"token.oauth.refresh\" on the connection";

/// The OAuth tokens of a connection.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    /// The access token (`token`).
    pub access_token: String,
    /// The refresh token (`token.oauth.refresh`).
    pub refresh_token: Option<String>,
    /// When the access token expires, as a Unix timestamp in seconds (`token.oauth.expiry`).
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl OAuthTokens {
    /// The tokens of an OAuth connection; `None` for other token types.
    pub fn from_connection(conn: &GitLabConnection) -> Option<Self> {
        (conn.token_type == TokenType::Oauth).then(|| Self {
            access_token: conn.token.clone(),
            refresh_token: conn.token_oauth_refresh.clone().filter(|t| !t.is_empty()),
            expires_at: conn.token_oauth_expiry,
        })
    }

    /// Whether the access token expires within `margin` seconds of `now` (false when the expiry
    /// is unknown).
    pub fn expires_within(&self, now: i64, margin: i64) -> bool {
        self.expires_at.is_some_and(|at| at - margin <= now)
    }

    /// Whether the token should be refreshed before use: it expires soon and can be refreshed.
    fn needs_refresh(&self, now: i64) -> bool {
        self.refresh_token.is_some() && self.expires_within(now, REFRESH_MARGIN)
    }
}

/// Exchanges a connection's refresh token for new tokens and stores them.
#[async_trait]
pub trait TokenRefresher: Send + Sync + std::fmt::Debug {
    /// Tokens to replace `stale`: tokens another process has stored since, or new ones from
    /// GitLab (stored before returning). [`CodeHostError::Unauthorized`] means GitLab rejected
    /// the refresh token.
    async fn refresh(&self, stale: &OAuthTokens) -> Result<OAuthTokens>;
}

struct State {
    current: OAuthTokens,
    /// Access tokens this session has replaced, newest last.
    seen: VecDeque<String>,
    /// Set when GitLab rejected the refresh token.
    failed: bool,
}

/// The current OAuth tokens of one connection, shared by every client of the connection in a
/// process.
pub struct TokenSession {
    refresher: Arc<dyn TokenRefresher>,
    state: Mutex<State>,
    /// Held while refreshing, so refreshes run one at a time.
    refreshing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for TokenSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("TokenSession")
            .field("current", &s.current)
            .field("failed", &s.failed)
            .finish_non_exhaustive()
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl TokenSession {
    /// A session starting with `tokens`.
    pub fn new(tokens: OAuthTokens, refresher: Arc<dyn TokenRefresher>) -> Self {
        Self {
            refresher,
            state: Mutex::new(State {
                current: tokens,
                seen: VecDeque::new(),
                failed: false,
            }),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current tokens.
    pub fn current(&self) -> OAuthTokens {
        self.lock().current.clone()
    }

    /// Takes `tokens` read from the connection's configuration: they replace the session's
    /// tokens (and clear a failed refresh) unless the session has already replaced them.
    pub fn observe(&self, tokens: &OAuthTokens) {
        let mut s = self.lock();
        if s.current.access_token == tokens.access_token {
            if s.current.refresh_token != tokens.refresh_token && tokens.refresh_token.is_some() {
                s.current.refresh_token.clone_from(&tokens.refresh_token);
                s.current.expires_at = tokens.expires_at;
                s.failed = false;
            }
            return;
        }
        if s.seen.contains(&tokens.access_token) {
            return;
        }
        s.current = tokens.clone();
        s.failed = false;
    }

    /// The access token to send, refreshed first when it expires within [`REFRESH_MARGIN`].
    /// When that refresh fails for a reason other than a rejected refresh token and the token
    /// has not expired yet, the current token is used.
    pub async fn access_token(&self) -> Result<String> {
        {
            let s = self.lock();
            if s.failed {
                return Err(CodeHostError::Config(REFRESH_REJECTED.to_owned()));
            }
            if !s.current.needs_refresh(now()) {
                return Ok(s.current.access_token.clone());
            }
        }
        let _guard = self.refreshing.lock().await;
        let stale = {
            let s = self.lock();
            if s.failed {
                return Err(CodeHostError::Config(REFRESH_REJECTED.to_owned()));
            }
            if !s.current.needs_refresh(now()) {
                return Ok(s.current.access_token.clone());
            }
            s.current.clone()
        };
        match self.refresh_locked(&stale).await {
            Ok(t) => Ok(t),
            Err(e) if !self.lock().failed && !stale.expires_within(now(), 0) => {
                tracing::warn!(error = %e, "refreshing the GitLab OAuth token failed; using the current one");
                Ok(stale.access_token)
            }
            Err(e) => Err(e),
        }
    }

    /// The access token to retry with after GitLab rejected `rejected` with a 401: the token
    /// that already replaced it, or a newly refreshed one. Fails with
    /// [`CodeHostError::Unauthorized`] when the session cannot refresh.
    pub async fn after_unauthorized(&self, rejected: &str) -> Result<String> {
        let _guard = self.refreshing.lock().await;
        let stale = {
            let s = self.lock();
            if s.failed {
                return Err(CodeHostError::Config(REFRESH_REJECTED.to_owned()));
            }
            if s.current.access_token != rejected {
                return Ok(s.current.access_token.clone());
            }
            if s.current.refresh_token.is_none() {
                return Err(CodeHostError::Unauthorized);
            }
            s.current.clone()
        };
        self.refresh_locked(&stale).await
    }

    /// Refreshes `stale`; the caller holds `refreshing`.
    async fn refresh_locked(&self, stale: &OAuthTokens) -> Result<String> {
        match self.refresher.refresh(stale).await {
            Ok(new) => {
                let mut s = self.lock();
                let old = std::mem::replace(&mut s.current, new);
                if old.access_token != s.current.access_token {
                    s.seen.push_back(old.access_token);
                    if s.seen.len() > SEEN_TOKENS {
                        s.seen.pop_front();
                    }
                }
                Ok(s.current.access_token.clone())
            }
            Err(CodeHostError::Unauthorized) => {
                tracing::warn!("GitLab rejected the connection's OAuth refresh token");
                self.lock().failed = true;
                Err(CodeHostError::Config(REFRESH_REJECTED.to_owned()))
            }
            Err(e) => Err(e),
        }
    }
}
