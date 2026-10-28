//! Sign-in with GitHub: the OAuth 2.0 authorization-code flow of a GitHub OAuth app, with
//! PKCE, implementing [`OAuthLoginProvider`].
//!
//! - `GET {base}/login/oauth/authorize` with the CSRF `state` and an S256 code challenge;
//!   `POST {base}/login/oauth/access_token` (`Accept: application/json`) exchanges the code
//!   (with the verifier) or a refresh token. A bad code is answered with `200` and an `error`
//!   field. OAuth app tokens do not expire; tokens that do come with a refresh token.
//! - Scopes: `read:user` and `user:email` (who signed in, and their verified emails), plus
//!   `read:org` when organization membership is checked (private memberships are only listed
//!   with it), and `repo` when a connection to the instance syncs repository permissions
//!   (listing a user's private repositories needs it).
//! - `GET /user` and `GET /user/emails` describe the user; `GET /user/orgs` lists their
//!   organizations.

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

use crate::client::{GitHubClient, PER_PAGE};
use crate::config::normalize_base_url;

/// The scopes requested at sign-in.
pub const SCOPES: &[&str] = &["read:user", "user:email"];
/// The scope added when organization membership is checked.
pub const ORG_SCOPE: &str = "read:org";
/// The scope added when repository permissions are synced with the user's token.
pub const REPO_SCOPE: &str = crate::authz::REPO_SCOPE;
/// The path of the OAuth callback on Sourcegull, relative to `externalURL`.
pub const CALLBACK_PATH: &str = ".auth/github/callback";

/// The GitHub OAuth app of one `auth.providers` entry.
#[derive(Clone)]
pub struct GitHubOAuth {
    base: Url,
    client_id: String,
    client_secret: String,
    redirect_uri: Url,
    scopes: Vec<&'static str>,
    http: reqwest::Client,
}

impl std::fmt::Debug for GitHubOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubOAuth")
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
    name: Option<String>,
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
    login: String,
}

impl GitHubOAuth {
    /// The OAuth app `client_id` on the instance at `url` (github.com or a GitHub Enterprise
    /// Server), redirecting back to `{external_url}/.auth/github/callback`. `orgs` adds the
    /// `read:org` scope for [`user_orgs`](Self::user_orgs).
    pub fn new(
        url: &str,
        client_id: &str,
        client_secret: &str,
        external_url: &str,
        orgs: bool,
    ) -> Result<Self> {
        let base = normalize_base_url(url).map_err(CodeHostError::Config)?;
        let redirect_uri = normalize_base_url(external_url)
            .and_then(|u| u.join(CALLBACK_PATH).map_err(|e| e.to_string()))
            .map_err(|e| CodeHostError::Config(format!("externalURL: {e}")))?;
        let mut scopes = SCOPES.to_vec();
        if orgs {
            scopes.push(ORG_SCOPE);
        }
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
            scopes,
            http,
        })
    }

    /// The app requesting the `repo` scope too, for repository permissions.
    #[must_use]
    pub fn with_repo_scope(mut self) -> Self {
        if !self.scopes.contains(&REPO_SCOPE) {
            self.scopes.push(REPO_SCOPE);
        }
        self
    }

    /// The instance's normalized base URL (the `service_id` of its accounts).
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    /// The OAuth app's client id.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn api(&self, access_token: &str) -> Result<GitHubClient> {
        GitHubClient::with_parts(
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

    /// Logins of the organizations the token's user belongs to.
    pub async fn user_orgs(&self, access_token: &str) -> Result<Vec<String>> {
        let client = self.api(access_token)?;
        let first = client.api_url(&format!("user/orgs?per_page={PER_PAGE}"))?;
        let orgs: Vec<OrgResponse> = client.paginate(first).try_collect().await?;
        Ok(orgs.into_iter().map(|o| o.login).collect())
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
impl OAuthLoginProvider for GitHubOAuth {
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

    /// The email is the primary one when GitHub has verified it, else the public profile
    /// email (unverified).
    async fn user_info(&self, access_token: &str) -> Result<OAuthUser> {
        let client = self.api(access_token)?;
        let u: UserResponse = client.get("user").await?;
        let emails: Vec<EmailResponse> = match client.get("user/emails").await {
            Ok(e) => e,
            // Not granted `user:email`.
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
            display_name: u.name.filter(|n| !n.is_empty()),
            email,
            email_verified,
            avatar_url: u.avatar_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_and_callback() {
        let o = GitHubOAuth::new(
            "https://github.com",
            "app",
            "secret",
            "https://sourcegull.example.com",
            true,
        )
        .unwrap();
        let url = o.authorize_url("st", "ch").unwrap();
        assert_eq!(url.host_str(), Some("github.com"));
        assert_eq!(url.path(), "/login/oauth/authorize");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "app");
        assert_eq!(
            q["redirect_uri"],
            "https://sourcegull.example.com/.auth/github/callback"
        );
        assert_eq!(q["scope"], "read:user user:email read:org");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(format!("{o:?}").find("secret").is_none());

        let ghes = GitHubOAuth::new(
            "https://ghe.example.com/",
            "app",
            "secret",
            "https://sourcegull.example.com/",
            false,
        )
        .unwrap();
        let url = ghes.authorize_url("st", "ch").unwrap();
        assert!(
            url.as_str()
                .starts_with("https://ghe.example.com/login/oauth/authorize?")
        );
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "read:user user:email");

        let url = ghes.with_repo_scope().authorize_url("st", "ch").unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "read:user user:email repo");
    }
}
