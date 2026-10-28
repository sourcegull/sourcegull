//! GitHub App authentication: installation tokens for connections with `githubApp`.
//!
//! An installation token is minted by `POST /app/installations/{id}/access_tokens`,
//! authenticated with a JWT signed (RS256) with the App's private key: `iss` is the App ID,
//! `iat` 60 seconds in the past (clock drift), `exp` 9 minutes later (GitHub allows at most
//! 10). GitHub issues tokens valid for one hour.
//!
//! Tokens are cached per connection in the process ([`AppTokens::shared`]), so every client of
//! the connection uses the same token. A token is replaced when less than
//! [`REFRESH_MARGIN`] of its life is left, or when GitHub rejects it (401). Refreshes are
//! single-flight: one request mints, concurrent requests wait for its token.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, RequestBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sourcegull_codehost::rest::{HeaderAuth, RestAuth, RestClient, decode};
use sourcegull_codehost::{CodeHostError, Result};
use url::Url;

use crate::config::GitHubApp;

/// A token with less than this much life left is replaced before use.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// How far `iat` is set in the past, to allow for clock drift.
const JWT_BACKDATE_SECS: i64 = 60;
/// The JWT's lifetime from `iat` (GitHub rejects more than 10 minutes).
const JWT_LIFETIME_SECS: i64 = 9 * 60;

/// Parses an App private key (PKCS#1 or PKCS#8 PEM).
pub fn encoding_key(pem: &str) -> std::result::Result<EncodingKey, String> {
    EncodingKey::from_rsa_pem(pem.trim().as_bytes())
        .map_err(|e| format!("not an RSA private key in PEM format: {e}"))
}

/// The claims of an App JWT.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppClaims {
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
}

/// An App JWT issued at `now` (backdated by a minute).
pub fn app_jwt(app_id: u64, key: &EncodingKey, now: DateTime<Utc>) -> Result<String> {
    let iat = now.timestamp() - JWT_BACKDATE_SECS;
    let claims = AppClaims {
        iss: app_id.to_string(),
        iat,
        exp: iat + JWT_LIFETIME_SECS,
    };
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, key)
        .map_err(|e| CodeHostError::Config(format!("signing the GitHub App JWT: {e}")))
}

/// An installation token and its expiry.
#[derive(Clone, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl InstallationToken {
    fn fresh(&self, now: DateTime<Utc>) -> bool {
        let margin = chrono::Duration::from_std(REFRESH_MARGIN).unwrap_or_default();
        self.expires_at - now > margin
    }
}

/// The installation tokens of one App installation (one connection).
pub struct AppTokens {
    app_id: u64,
    installation_id: u64,
    key: EncodingKey,
    current: Mutex<Option<InstallationToken>>,
    /// Held while minting, so only one request mints at a time.
    refresh: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for AppTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppTokens")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .finish_non_exhaustive()
    }
}

/// The token caches of the process, by API URL and App credentials.
static SHARED: LazyLock<Mutex<HashMap<String, Arc<AppTokens>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl AppTokens {
    /// A cache of its own for `app` (tests; production code uses [`shared`](Self::shared)).
    pub fn new(app: &GitHubApp) -> Result<Self> {
        Ok(Self {
            app_id: app.app_id,
            installation_id: app.installation_id,
            key: encoding_key(&app.private_key).map_err(CodeHostError::Config)?,
            current: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    /// The process-wide cache of `app` on the instance whose API is at `api`: every client of
    /// the same connection shares its tokens. Caches that no client holds any more and whose
    /// token has expired are dropped.
    pub fn shared(api: &Url, app: &GitHubApp) -> Result<Arc<Self>> {
        let mut h = Sha256::new();
        h.update(api.as_str());
        h.update([0]);
        h.update(app.app_id.to_be_bytes());
        h.update(app.installation_id.to_be_bytes());
        h.update(app.private_key.trim());
        let id = hex::encode(h.finalize());
        let mut map = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = map.get(&id) {
            return Ok(t.clone());
        }
        let now = Utc::now();
        map.retain(|_, t| {
            Arc::strong_count(t) > 1
                || t.lock_current()
                    .as_ref()
                    .is_some_and(|c| c.expires_at > now)
        });
        let t = Arc::new(Self::new(app)?);
        map.insert(id, t.clone());
        Ok(t)
    }

    fn lock_current(&self) -> std::sync::MutexGuard<'_, Option<InstallationToken>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached token when it has more than [`REFRESH_MARGIN`] left.
    pub fn cached(&self) -> Option<String> {
        self.lock_current()
            .as_ref()
            .filter(|t| t.fresh(Utc::now()))
            .map(|t| t.token.clone())
    }

    /// A token with more than [`REFRESH_MARGIN`] left, minted with `rest` (a client of the
    /// instance without credentials) when the cached one is older.
    pub async fn token(&self, rest: &RestClient) -> Result<String> {
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let _minting = self.refresh.lock().await;
        // Another request may have minted while this one waited.
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let minted = self.mint(rest).await?;
        let token = minted.token.clone();
        *self.lock_current() = Some(minted);
        Ok(token)
    }

    /// Forgets `rejected` when it is the cached token, so the next request mints a new one.
    pub fn invalidate(&self, rejected: &str) {
        let mut current = self.lock_current();
        if current.as_ref().is_some_and(|t| t.token == rejected) {
            *current = None;
        }
    }

    async fn mint(&self, rest: &RestClient) -> Result<InstallationToken> {
        let jwt = app_jwt(self.app_id, &self.key, Utc::now())?;
        let url = rest
            .origin()
            .join(&format!(
                "app/installations/{}/access_tokens",
                self.installation_id
            ))
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        let resp = rest
            .with_auth(Arc::new(HeaderAuth::bearer(&jwt)?))
            .send(Method::POST, url, None)
            .await
            .map_err(|e| match e {
                CodeHostError::Unauthorized => CodeHostError::Config(format!(
                    "GitHub rejected the JWT of App {}: check appID and privateKey",
                    self.app_id
                )),
                CodeHostError::NotFound(_) => CodeHostError::Config(format!(
                    "installation {} of GitHub App {} not found: check installationID",
                    self.installation_id, self.app_id
                )),
                other => other,
            })?;
        let token: InstallationToken = decode(resp).await?;
        tracing::debug!(
            app_id = self.app_id,
            installation_id = self.installation_id,
            expires_at = %token.expires_at,
            "minted a GitHub App installation token"
        );
        Ok(token)
    }
}

/// Authenticates requests with the connection's installation token.
#[derive(Debug)]
pub struct AppAuth {
    tokens: Arc<AppTokens>,
    /// A client of the instance without credentials, for minting.
    minting: RestClient,
}

impl AppAuth {
    pub fn new(tokens: Arc<AppTokens>, minting: RestClient) -> Self {
        Self { tokens, minting }
    }

    /// A current installation token (minted when needed).
    pub async fn token(&self) -> Result<String> {
        self.tokens.token(&self.minting).await
    }

    /// The cached token, if it is current.
    pub fn cached(&self) -> Option<String> {
        self.tokens.cached()
    }
}

#[async_trait::async_trait]
impl RestAuth for AppAuth {
    async fn authorize(&self, req: RequestBuilder) -> Result<(RequestBuilder, Option<String>)> {
        let token = self.token().await?;
        Ok((req.bearer_auth(&token), Some(token)))
    }

    async fn after_unauthorized(&self, rejected: &str) -> Result<bool> {
        // The token was revoked or expired early: retry once with a new one.
        self.tokens.invalidate(rejected);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation};

    const KEY: &str = include_str!("testdata/app-key.pem");
    const PUBLIC_KEY: &str = include_str!("testdata/app-key.pub.pem");

    #[test]
    fn jwt_is_rs256_with_backdated_iat() {
        let now = Utc::now();
        let jwt = app_jwt(42, &encoding_key(KEY).unwrap(), now).unwrap();
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&["42"]);
        let claims = jsonwebtoken::decode::<AppClaims>(
            &jwt,
            &DecodingKey::from_rsa_pem(PUBLIC_KEY.as_bytes()).unwrap(),
            &v,
        )
        .unwrap()
        .claims;
        assert_eq!(claims.iat, now.timestamp() - 60);
        assert!(claims.exp - claims.iat <= 600);
        assert!(claims.exp > now.timestamp());
    }

    #[test]
    fn rejects_keys_that_are_not_rsa_pem() {
        assert!(encoding_key("not a key").is_err());
        assert!(encoding_key(KEY).is_ok());
        // `${file:…}` drops trailing newlines.
        assert!(encoding_key(KEY.trim_end()).is_ok());
    }

    #[test]
    fn freshness_keeps_a_margin() {
        let now = Utc::now();
        let t = |mins| InstallationToken {
            token: "t".into(),
            expires_at: now + chrono::Duration::minutes(mins),
        };
        assert!(t(60).fresh(now));
        assert!(!t(4).fresh(now));
        assert!(!t(-1).fresh(now));
    }
}
