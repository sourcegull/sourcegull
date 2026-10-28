//! Sign-in with the code host: [`OAuthLoginProvider`], and [`parse_token_response`] for the
//! token endpoints.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{CodeHostError, Result};

/// Tokens returned by the code host's OAuth endpoint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// The signed-in user as the code host describes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthUser {
    pub account_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub email_verified: bool,
    pub avatar_url: Option<String>,
}

/// The OAuth 2.0 authorization-code flow against a code host.
#[async_trait]
pub trait OAuthLoginProvider: Send + Sync {
    /// The URL to send the browser to. `state` is the CSRF token; `pkce_challenge` the S256
    /// code challenge.
    fn authorize_url(&self, state: &str, pkce_challenge: &str) -> Result<url::Url>;
    /// Exchanges the authorization code for tokens.
    async fn exchange(&self, code: &str, pkce_verifier: &str) -> Result<OAuthToken>;
    /// Refreshes an access token (done 5 minutes before expiry, single-flight per account).
    async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken>;
    /// Loads the user behind an access token.
    async fn user_info(&self, access_token: &str) -> Result<OAuthUser>;
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    error: Option<String>,
}

/// Decodes the response of an OAuth token endpoint (authorization code or refresh grant).
/// A 400 or 401, or a success carrying `error` (some code hosts answer a bad code with 200),
/// is [`CodeHostError::Unauthorized`]. `scope` may be space- or comma-separated; the expiry
/// counts from now.
pub fn parse_token_response(status: u16, body: &[u8]) -> Result<OAuthToken> {
    if status == 400 || status == 401 {
        // invalid_grant (expired or reused code, revoked refresh token) or a bad client.
        return Err(CodeHostError::Unauthorized);
    }
    if !(200..300).contains(&status) {
        return Err(CodeHostError::Http {
            status,
            message: String::from_utf8_lossy(body).chars().take(500).collect(),
        });
    }
    let t: TokenResponse =
        serde_json::from_slice(body).map_err(|e| CodeHostError::Decode(e.to_string()))?;
    let Some(access_token) = t.access_token.filter(|_| t.error.is_none()) else {
        return Err(CodeHostError::Unauthorized);
    };
    Ok(OAuthToken {
        access_token,
        refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
        expires_at: t
            .expires_in
            .filter(|s| *s > 0)
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
        scopes: t
            .scope
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_responses() {
        let t = parse_token_response(
            200,
            br#"{"access_token":"a","token_type":"bearer","scope":"read:org,user:email"}"#,
        )
        .unwrap();
        assert_eq!(t.access_token, "a");
        assert_eq!(t.scopes, ["read:org", "user:email"]);
        assert!(t.refresh_token.is_none() && t.expires_at.is_none());

        let t = parse_token_response(
            200,
            br#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":"read:user read:organization"}"#,
        )
        .unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        assert!(t.expires_at.unwrap() > Utc::now());
        assert_eq!(t.scopes.len(), 2);

        assert!(matches!(
            parse_token_response(200, br#"{"error":"bad_verification_code"}"#),
            Err(CodeHostError::Unauthorized)
        ));
        assert!(matches!(
            parse_token_response(400, b"{}"),
            Err(CodeHostError::Unauthorized)
        ));
        assert!(matches!(
            parse_token_response(502, b"bad gateway"),
            Err(CodeHostError::Http { status: 502, .. })
        ));
    }
}
