//! Sign-in with Gitea: the OAuth 2.0 authorization-code flow of a Gitea OAuth2 application,
//! with PKCE, implementing [`OAuthLoginProvider`].
//!
//! - `GET {base}/login/oauth/authorize` with `response_type=code`, the CSRF `state` and an
//!   S256 code challenge; `POST {base}/login/oauth/access_token` exchanges the code (with the
//!   verifier) or a refresh token. Gitea access tokens expire (an hour by default) and come
//!   with a refresh token.
//! - Scopes: `read:user` (who signed in, and their emails) and `read:organization`
//!   (organization membership), plus `read:repository` when a Gitea connection of the same
//!   instance syncs repository permissions with the users' tokens
//!   ([`GiteaOAuth::with_repo_scope`]).
//! - `GET /api/v1/user` and `GET /api/v1/user/emails` describe the user; `GET
//!   /api/v1/user/orgs` lists their organizations.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;
use serde::Deserialize;
use sourcegull_codehost::{
    ClientOptions, CodeHostError, OAuthLoginProvider, OAuthToken, OAuthUser, Result, TokenBucket,
    parse_token_response,
};
use url::Url;

use crate::client::GiteaClient;
use crate::config::normalize_base_url;

/// The scopes requested at sign-in.
pub const SCOPES: &[&str] = &["read:user", "read:organization"];
/// The scope added when repository permissions are read with the user's token.
pub const REPO_SCOPE: &str = "read:repository";
/// The path of the OAuth callback on Sourcegull, relative to `externalURL`.
pub const CALLBACK_PATH: &str = ".auth/gitea/callback";

/// The Gitea OAuth2 application of one `auth.providers` entry.
#[derive(Clone)]
pub struct GiteaOAuth {
    base: Url,
    client_id: String,
    client_secret: String,
    redirect_uri: Url,
    scopes: Vec<&'static str>,
    http: reqwest::Client,
}

impl std::fmt::Debug for GiteaOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GiteaOAuth")
            .field("base", &self.base.as_str())
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri.as_str())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct UserResponse {
    id: i64,
    login: String,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct EmailResponse {
    email: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    verified: bool,
}

#[derive(Deserialize)]
struct OrgResponse {
    #[serde(default)]
    name: Option<String>,
    /// The name, under its older field.
    #[serde(default)]
    username: Option<String>,
}

impl GiteaOAuth {
    /// The application `client_id` on the instance at `url`, redirecting back to
    /// `{external_url}/.auth/gitea/callback`.
    pub fn new(
        url: &str,
        client_id: &str,
        client_secret: &str,
        external_url: &str,
    ) -> Result<Self> {
        let base = normalize_base_url(url).map_err(CodeHostError::Config)?;
        let redirect_uri = normalize_base_url(external_url)
            .and_then(|u| u.join(CALLBACK_PATH).map_err(|e| e.to_string()))
            .map_err(|e| CodeHostError::Config(format!("externalURL: {e}")))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("sourcegull/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| CodeHostError::Config(format!("HTTP client: {e}")))?;
        Ok(Self {
            base,
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            redirect_uri,
            scopes: SCOPES.to_vec(),
            http,
        })
    }

    /// This application also requesting [`REPO_SCOPE`] when `repos` is set, for the
    /// permissions sync of connections with `identityProvider: oauth`.
    #[must_use]
    pub fn with_repo_scope(mut self, repos: bool) -> Self {
        if repos && !self.scopes.contains(&REPO_SCOPE) {
            self.scopes.push(REPO_SCOPE);
        }
        self
    }

    /// The instance's normalized base URL (the `service_id` of its accounts).
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The OAuth2 application's client id.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn api(&self, access_token: &str) -> Result<GiteaClient> {
        GiteaClient::with_parts(
            self.base.as_str(),
            Some(access_token.to_owned()),
            None,
            Arc::new(TokenBucket::unlimited()),
            ClientOptions {
                timeout: Duration::from_secs(30),
                ..ClientOptions::default()
            },
        )
    }

    /// Names of the organizations the token's user belongs to.
    pub async fn user_orgs(&self, access_token: &str) -> Result<Vec<String>> {
        let client = self.api(access_token)?;
        let orgs: Vec<OrgResponse> = client
            .paginate(client.api_url("user/orgs")?)
            .try_collect()
            .await?;
        Ok(orgs
            .into_iter()
            .filter_map(|o| o.name.filter(|n| !n.is_empty()).or(o.username))
            .collect())
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<OAuthToken> {
        let url = self
            .base
            .join("login/oauth/access_token")
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        let resp = self
            .http
            .post(url)
            .header("accept", "application/json")
            .form(form)
            .send()
            .await
            .map_err(|e| CodeHostError::Transport(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| CodeHostError::Transport(e.without_url().to_string()))?;
        parse_token_response(status, &body)
    }
}

#[async_trait]
impl OAuthLoginProvider for GiteaOAuth {
    fn authorize_url(&self, state: &str, pkce_challenge: &str) -> Result<Url> {
        let mut url = self
            .base
            .join("login/oauth/authorize")
            .map_err(|e| CodeHostError::Config(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", self.redirect_uri.as_str())
            .append_pair("response_type", "code")
            .append_pair("state", state)
            .append_pair("scope", &self.scopes.join(" "))
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
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", pkce_verifier),
        ])
        .await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken> {
        self.token(&[
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .await
    }

    /// The email is the primary one when Gitea has verified it, else the profile email
    /// (unverified).
    async fn user_info(&self, access_token: &str) -> Result<OAuthUser> {
        let client = self.api(access_token)?;
        let u: UserResponse = client.get("user").await?;
        let emails: Vec<EmailResponse> = match client.get("user/emails").await {
            Ok(e) => e,
            Err(CodeHostError::NotFound(_) | CodeHostError::Forbidden(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        let verified = emails.into_iter().find(|e| e.primary && e.verified);
        let (email, email_verified) = match verified {
            Some(e) => (Some(e.email), true),
            None => (u.email.filter(|e| !e.is_empty()), false),
        };
        Ok(OAuthUser {
            account_id: u.id.to_string(),
            username: u.login,
            display_name: u.full_name.filter(|n| !n.is_empty()),
            email,
            email_verified,
            avatar_url: u.avatar_url.filter(|a| !a.is_empty()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_and_callback() {
        let o = GiteaOAuth::new(
            "https://gitea.example.com/git",
            "app",
            "secret",
            "https://sourcegull.example.com",
        )
        .unwrap();
        let url = o.authorize_url("st", "ch").unwrap();
        assert_eq!(url.path(), "/git/login/oauth/authorize");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "app");
        assert_eq!(
            q["redirect_uri"],
            "https://sourcegull.example.com/.auth/gitea/callback"
        );
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "read:user read:organization");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(format!("{o:?}").find("secret").is_none());

        let url = o.with_repo_scope(true).authorize_url("st", "ch").unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "read:user read:organization read:repository");
    }
}
