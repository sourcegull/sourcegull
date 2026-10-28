//! Sign-in with GitLab: the OAuth 2.0 authorization-code flow with PKCE, implementing
//! [`OAuthLoginProvider`].
//!
//! - `GET {base}/oauth/authorize` with `response_type=code`, the CSRF `state` and an S256 code
//!   challenge; `POST {base}/oauth/token` exchanges the code (with the verifier) or a refresh
//!   token. GitLab access tokens live two hours and come with a refresh token.
//! - Scopes: `read_user` (who signed in) and `read_api` (group membership for `allowGroups`
//!   and the user-centric permissions sync).
//! - `GET /api/v4/user` describes the user; `GET /api/v4/groups?min_access_level=10` lists
//!   their groups.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use serde::Deserialize;
use sourcegull_codehost::{
    CodeHostError, OAuthLoginProvider, OAuthToken, OAuthUser, Result, TokenBucket,
};
use url::Url;

use crate::client::{Auth, ClientOptions, GitLabClient, PER_PAGE};
use crate::config::normalize_base_url;

/// The scopes requested at sign-in.
pub const SCOPES: &[&str] = &["read_user", "read_api"];
/// The path of the OAuth callback on Sourcegull, relative to `externalURL`.
pub const CALLBACK_PATH: &str = ".auth/gitlab/callback";

/// The GitLab OAuth application of one `auth.providers` entry.
#[derive(Clone)]
pub struct GitLabOAuth {
    base: Url,
    client_id: String,
    client_secret: String,
    /// The sign-in callback; `None` for an application used only to refresh tokens.
    redirect_uri: Option<Url>,
    http: reqwest::Client,
    api: Arc<GitLabClient>,
}

impl std::fmt::Debug for GitLabOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabOAuth")
            .field("base", &self.base.as_str())
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri.as_ref().map(Url::as_str))
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct UserResponse {
    id: i64,
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default)]
    confirmed_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct GroupResponse {
    full_path: String,
}

impl GitLabOAuth {
    /// The application `client_id` on the instance at `url`, redirecting back to
    /// `{external_url}/.auth/gitlab/callback`.
    pub fn new(
        url: &str,
        client_id: &str,
        client_secret: &str,
        external_url: &str,
    ) -> Result<Self> {
        let redirect_uri = normalize_base_url(external_url)
            .and_then(|u| u.join(CALLBACK_PATH).map_err(|e| e.to_string()))
            .map_err(|e| CodeHostError::Config(format!("externalURL: {e}")))?;
        Self::build(url, client_id, client_secret, Some(redirect_uri), None)
    }

    /// The application `client_id` on the instance at `url`, used only to refresh tokens
    /// ([`refresh`](OAuthLoginProvider::refresh)), trusting `certificate` (PEM) in addition to
    /// the system roots.
    pub fn for_refresh(
        url: &str,
        client_id: &str,
        client_secret: &str,
        certificate: Option<&str>,
    ) -> Result<Self> {
        Self::build(url, client_id, client_secret, None, certificate)
    }

    fn build(
        url: &str,
        client_id: &str,
        client_secret: &str,
        redirect_uri: Option<Url>,
        certificate: Option<&str>,
    ) -> Result<Self> {
        let base = normalize_base_url(url).map_err(CodeHostError::Config)?;
        let opts = ClientOptions {
            timeout: Duration::from_secs(30),
            ..ClientOptions::default()
        };
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("sourcegull/", env!("CARGO_PKG_VERSION")))
            .timeout(opts.timeout);
        if let Some(pem) = certificate {
            let cert = reqwest::Certificate::from_pem(pem.as_bytes())
                .map_err(|e| CodeHostError::Config(format!("certificate: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let http = builder
            .build()
            .map_err(|e| CodeHostError::Config(format!("HTTP client: {e}")))?;
        let api = Arc::new(GitLabClient::with_parts(
            base.as_str(),
            Auth::None,
            certificate,
            Arc::new(TokenBucket::unlimited()),
            opts,
        )?);
        Ok(Self {
            base,
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            redirect_uri,
            http,
            api,
        })
    }

    /// The instance's normalized base URL (the `service_id` of its accounts).
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The OAuth application id.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn callback(&self) -> Result<&Url> {
        self.redirect_uri.as_ref().ok_or_else(|| {
            CodeHostError::Config("the OAuth application has no sign-in callback".to_owned())
        })
    }

    /// Full paths of the groups the token's user belongs to (any access level).
    pub async fn user_groups(&self, access_token: &str) -> Result<Vec<String>> {
        let client = self.api.with_auth(Auth::Bearer(access_token.to_owned()));
        let first = client.api_url(&format!(
            "groups?min_access_level=10&per_page={PER_PAGE}&order_by=id&sort=asc"
        ))?;
        let groups: Vec<GroupResponse> = client.paginate(first).try_collect().await?;
        Ok(groups.into_iter().map(|g| g.full_path).collect())
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<OAuthToken> {
        let url = self
            .base
            .join("oauth/token")
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        let resp = self
            .http
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| CodeHostError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| CodeHostError::Transport(e.without_url().to_string()))?;
        if status.as_u16() == 400 || status.as_u16() == 401 {
            // invalid_grant (expired or reused code, revoked refresh token) or a bad client.
            return Err(CodeHostError::Unauthorized);
        }
        if !status.is_success() {
            return Err(CodeHostError::Http {
                status: status.as_u16(),
                message: String::from_utf8_lossy(&body).chars().take(500).collect(),
            });
        }
        let t: TokenResponse =
            serde_json::from_slice(&body).map_err(|e| CodeHostError::Decode(e.to_string()))?;
        let issued = t
            .created_at
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .unwrap_or_else(Utc::now);
        Ok(OAuthToken {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: t
                .expires_in
                .map(|secs| issued + chrono::Duration::seconds(secs)),
            scopes: t.scope.split_whitespace().map(str::to_owned).collect(),
        })
    }
}

#[async_trait]
impl OAuthLoginProvider for GitLabOAuth {
    fn authorize_url(&self, state: &str, pkce_challenge: &str) -> Result<Url> {
        let mut url = self
            .base
            .join("oauth/authorize")
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", self.callback()?.as_str())
            .append_pair("response_type", "code")
            .append_pair("state", state)
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("code_challenge", pkce_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    async fn exchange(&self, code: &str, pkce_verifier: &str) -> Result<OAuthToken> {
        self.token(&[
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", self.callback()?.as_str()),
            ("code_verifier", pkce_verifier),
        ])
        .await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken> {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        if let Some(uri) = &self.redirect_uri {
            form.push(("redirect_uri", uri.as_str()));
        }
        self.token(&form).await
    }

    async fn user_info(&self, access_token: &str) -> Result<OAuthUser> {
        let client = self.api.with_auth(Auth::Bearer(access_token.to_owned()));
        let u: UserResponse = client.get("user").await?;
        Ok(OAuthUser {
            account_id: u.id.to_string(),
            username: u.username,
            display_name: u.name.filter(|n| !n.is_empty()),
            email_verified: u.email.is_some() && u.confirmed_at.is_some(),
            email: u.email.filter(|e| !e.is_empty()),
            avatar_url: u.avatar_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_and_callback() {
        let o = GitLabOAuth::new(
            "https://gitlab.example.com/gl",
            "app",
            "secret",
            "https://sourcegull.example.com",
        )
        .unwrap();
        let url = o.authorize_url("st", "ch").unwrap();
        assert_eq!(url.path(), "/gl/oauth/authorize");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "app");
        assert_eq!(
            q["redirect_uri"],
            "https://sourcegull.example.com/.auth/gitlab/callback"
        );
        assert_eq!(q["scope"], "read_user read_api");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(format!("{o:?}").find("secret").is_none());
    }
}
