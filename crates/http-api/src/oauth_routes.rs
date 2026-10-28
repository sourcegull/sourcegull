//! Sign-in with code host OAuth (GitLab, GitHub and Gitea): `GET /.auth/<host>/login` and
//! `GET /.auth/<host>/callback`, where `<host>` is `gitlab`, `github` or `gitea`.
//!
//! `login?pc=<clientID>&returnTo=<path>` picks the `auth.providers` entry of that host with that
//! `clientID` (or the only one), stores a PKCE verifier under the hash of a random state, puts the
//! state in a short-lived `HttpOnly` cookie and redirects to the code host. The callback checks
//! that the `state` parameter equals the cookie and takes the stored flow (once; a flow started
//! for another host is refused), exchanges the code, loads the user, applies `allowGroups`
//! (GitLab) or `allowOrgs` (GitHub, Gitea), resolves or creates the Sourcegull user
//! ([`Authenticator::sign_in_external`]), stores the tokens encrypted, starts a session, queues a
//! permissions sync and redirects to `returnTo`. Failures redirect to `/sign-in?error=<message>`.

use std::time::Duration;

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sourcegull_authn::{AuthError, ExternalIdentity};
use sourcegull_codehost::{CodeHostError, OAuthLoginProvider, OAuthToken};
use sourcegull_codehost_gitea::GiteaOAuth;
use sourcegull_codehost_github::GitHubOAuth;
use sourcegull_codehost_gitlab::GitLabOAuth;
use sourcegull_config::AuthProvider;
use sourcegull_crypto::constant_time_eq;
use sourcegull_crypto::pkce;
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::OAuthFlow;
use sourcegull_http_extract::RequestAuth;
use sourcegull_services::AppState;
use sourcegull_types::ExternalServiceKind;
use url::Url;

use crate::auth_routes::{
    end_previous_session, external_url_is_https, queue_perms_sync, session_set_cookie,
};

/// Cookie holding the state of the flow in progress.
pub const STATE_COOKIE: &str = "sourcegull_oauth_state";
/// How long a flow may take.
pub const FLOW_TTL: Duration = Duration::from_secs(600);

/// A code host that users can sign in with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Gitlab,
    Github,
    Gitea,
}

impl Host {
    const ALL: [Host; 3] = [Host::Gitlab, Host::Github, Host::Gitea];

    /// The provider `type`, the path segment of its routes and the `service_type` of its
    /// accounts.
    fn kind(self) -> &'static str {
        match self {
            Host::Gitlab => sourcegull_codehost_gitlab::SERVICE_TYPE,
            Host::Github => sourcegull_codehost_github::SERVICE_TYPE,
            Host::Gitea => sourcegull_codehost_gitea::SERVICE_TYPE,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Host::Gitlab => "GitLab",
            Host::Github => "GitHub",
            Host::Gitea => "Gitea",
        }
    }

    /// One of what `allow` lists, with its article.
    fn membership(self) -> &'static str {
        match self {
            Host::Gitlab => "a group",
            Host::Github | Host::Gitea => "an organization",
        }
    }
}

/// One code host entry of `auth.providers`.
#[derive(Debug, Clone)]
struct Provider {
    host: Host,
    url: String,
    client_id: String,
    client_secret: String,
    allow_signup: bool,
    link_by_email: Option<bool>,
    /// `allowGroups` (GitLab) or `allowOrgs`.
    allow: Vec<String>,
    display_name: Option<String>,
}

impl Provider {
    fn from_config(p: &AuthProvider) -> Option<Self> {
        Some(match p {
            AuthProvider::Gitlab(g) => Self {
                host: Host::Gitlab,
                url: g.url.clone(),
                client_id: g.client_id.clone(),
                client_secret: g.client_secret.clone(),
                allow_signup: g.allow_signup,
                link_by_email: g.link_by_email,
                allow: g.allow_groups.clone(),
                display_name: g.display_name.clone(),
            },
            AuthProvider::Github(g) => Self {
                host: Host::Github,
                url: g.url.clone(),
                client_id: g.client_id.clone(),
                client_secret: g.client_secret.clone(),
                allow_signup: g.allow_signup,
                link_by_email: g.link_by_email,
                allow: g.allow_orgs.clone(),
                display_name: g.display_name.clone(),
            },
            AuthProvider::Gitea(g) => Self {
                host: Host::Gitea,
                url: g.url.clone(),
                client_id: g.client_id.clone(),
                client_secret: g.client_secret.clone(),
                allow_signup: g.allow_signup,
                link_by_email: g.link_by_email,
                allow: g.allow_orgs.clone(),
                display_name: g.display_name.clone(),
            },
            _ => return None,
        })
    }

    /// The flow's `provider`: host and client id, so a callback only takes its own host's
    /// flows.
    fn flow_key(&self) -> String {
        format!("{}:{}", self.host.kind(), self.client_id)
    }
}

/// The OAuth client of a [`Provider`].
enum Client {
    Gitlab(GitLabOAuth),
    Github(GitHubOAuth),
    Gitea(GiteaOAuth),
}

impl Client {
    fn login(&self) -> &dyn OAuthLoginProvider {
        match self {
            Client::Gitlab(c) => c,
            Client::Github(c) => c,
            Client::Gitea(c) => c,
        }
    }

    fn base_url(&self) -> &Url {
        match self {
            Client::Gitlab(c) => c.base_url(),
            Client::Github(c) => c.base_url(),
            Client::Gitea(c) => c.base_url(),
        }
    }

    /// The groups (GitLab) or organizations the token's user belongs to.
    async fn memberships(&self, access_token: &str) -> Result<Vec<String>, CodeHostError> {
        match self {
            Client::Gitlab(c) => c.user_groups(access_token).await,
            Client::Github(c) => c.user_orgs(access_token).await,
            Client::Gitea(c) => c.user_orgs(access_token).await,
        }
    }
}

/// The `/.auth/{gitlab,github,gitea}/*` routes. Merge into
/// [`public_routes`](crate::public_routes).
pub fn oauth_routes() -> Router<AppState> {
    Host::ALL.into_iter().fold(Router::new(), |r, host| {
        r.route(
            &format!("/.auth/{}/login", host.kind()),
            get(move |state, query| login(host, state, query)),
        )
        .route(
            &format!("/.auth/{}/callback", host.kind()),
            get(move |state, auth, headers, query| callback(host, state, auth, headers, query)),
        )
    })
}

/// The code host and OpenID Connect providers of `auth.providers` for the sign-in page:
/// `{type, displayName, authenticationURL}`.
pub fn provider_buttons(state: &AppState) -> Vec<serde_json::Value> {
    state
        .config
        .load()
        .value
        .auth_providers
        .iter()
        .filter_map(|p| match p {
            AuthProvider::OpenIdConnect(p) => Some(crate::oidc_routes::provider_button(p)),
            p => Provider::from_config(p).map(|p| {
                json!({
                    "type": p.host.kind(),
                    "displayName": p.display_name.clone().unwrap_or_else(|| p.host.name().to_owned()),
                    "authenticationURL": format!(
                        "/.auth/{}/login?pc={}",
                        p.host.kind(),
                        percent_encoding::utf8_percent_encode(&p.client_id, percent_encoding::NON_ALPHANUMERIC)
                    ),
                })
            }),
        })
        .collect()
}

/// The `host` provider with `client_id` (`None`: the only one).
fn find_provider(state: &AppState, host: Host, client_id: Option<&str>) -> Option<Provider> {
    let config = state.config.load();
    let mut providers = config
        .value
        .auth_providers
        .iter()
        .filter_map(Provider::from_config)
        .filter(|p| p.host == host);
    match client_id {
        Some(id) => providers.find(|p| p.client_id == id),
        None => {
            let first = providers.next()?;
            providers.next().is_none().then_some(first)
        }
    }
}

/// Whether a live `kind` connection to the instance at `base` (a normalized URL) has an
/// `authorization` block, so sign-in asks for the scopes its permissions sync reads with.
async fn syncs_permissions(state: &AppState, kind: ExternalServiceKind, base: &str) -> bool {
    let registry = sourcegull_repos::Registry::default();
    let services = state.db.external_services(state.keyring.clone());
    match services.list_with_config(kind).await {
        Ok(list) => list.iter().any(|(_, text)| {
            !registry.unrestricted(kind, text)
                && registry.service_id(kind, text).is_ok_and(|id| id == base)
        }),
        Err(e) => {
            tracing::warn!(error = %e, "listing connections for sign-in scopes failed");
            false
        }
    }
}

async fn oauth_client(state: &AppState, p: &Provider) -> Result<Client, String> {
    let external = state
        .config
        .load()
        .value
        .external_url
        .clone()
        .ok_or_else(|| {
            format!(
                "{} sign-in needs externalURL in the site configuration.",
                p.host.name()
            )
        })?;
    let (url, id, secret) = (&p.url, &p.client_id, &p.client_secret);
    match p.host {
        Host::Gitlab => GitLabOAuth::new(url, id, secret, &external).map(Client::Gitlab),
        Host::Github => match GitHubOAuth::new(url, id, secret, &external, !p.allow.is_empty()) {
            Ok(app) => {
                let repos =
                    syncs_permissions(state, ExternalServiceKind::GitHub, app.base_url().as_str())
                        .await;
                Ok(Client::Github(if repos {
                    app.with_repo_scope()
                } else {
                    app
                }))
            }
            Err(e) => Err(e),
        },
        Host::Gitea => match GiteaOAuth::new(url, id, secret, &external) {
            Ok(app) => {
                let repos =
                    syncs_permissions(state, ExternalServiceKind::Gitea, app.base_url().as_str())
                        .await;
                Ok(Client::Gitea(app.with_repo_scope(repos)))
            }
            Err(e) => Err(e),
        },
    }
    .map_err(|e| e.to_string())
}

/// A same-origin path, or `/`.
pub(crate) fn safe_return_to(raw: Option<&str>) -> String {
    match raw {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && !p.chars().any(char::is_control) =>
        {
            p.to_owned()
        }
        _ => "/".to_owned(),
    }
}

pub(crate) fn redirect(location: &str, cookies: &[HeaderValue]) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(location) {
        h.insert(header::LOCATION, v);
    }
    for c in cookies {
        h.append(header::SET_COOKIE, c.clone());
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(crate) fn state_cookie(state: &AppState, value: Option<&str>) -> HeaderValue {
    let (value, max_age) = match value {
        Some(v) => (v, FLOW_TTL.as_secs()),
        None => ("", 0),
    };
    let mut c =
        format!("{STATE_COOKIE}={value}; Path=/.auth/; Max-Age={max_age}; HttpOnly; SameSite=Lax");
    if external_url_is_https(state) {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c)
        .unwrap_or_else(|_| HeaderValue::from_static("sourcegull_oauth_state="))
}

/// Redirects to the sign-in page showing `message`, clearing the state cookie.
pub(crate) fn fail(state: &AppState, message: &str) -> Response {
    let q: String = url_encode(message);
    redirect(&format!("/sign-in?error={q}"), &[state_cookie(state, None)])
}

fn url_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    pc: Option<String>,
    #[serde(rename = "returnTo")]
    return_to: Option<String>,
}

/// `GET /.auth/<host>/login?pc=<clientID>&returnTo=<path>`.
async fn login(host: Host, State(state): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    let Some(provider) = find_provider(&state, host, q.pc.as_deref()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("Unknown {} auth provider.", host.name()),
                "code": "NOT_FOUND",
            })),
        )
            .into_response();
    };
    let client = match oauth_client(&state, &provider).await {
        Ok(c) => c,
        Err(e) => return fail(&state, &e),
    };
    let flow_state = generate_session_id();
    let verifier = pkce::generate_verifier();
    let url = match client
        .login()
        .authorize_url(&flow_state, &pkce::challenge_s256(&verifier))
    {
        Ok(u) => u,
        Err(e) => return fail(&state, &e.to_string()),
    };
    let flow = OAuthFlow {
        provider: provider.flow_key(),
        pkce_verifier: verifier,
        return_to: safe_return_to(q.return_to.as_deref()),
        nonce: None,
    };
    let expires = chrono::Utc::now() + chrono::Duration::from_std(FLOW_TTL).unwrap_or_default();
    let accounts = state.db.external_accounts(state.keyring.clone());
    if let Err(e) = accounts
        .create_flow(&hash_session_id(&flow_state), &flow, expires)
        .await
    {
        tracing::error!(error = %e, "storing OAuth flow");
        return fail(&state, "Sign-in failed. Try again.");
    }
    redirect(url.as_str(), &[state_cookie(&state, Some(&flow_state))])
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find_map(|(k, v)| (k == name).then_some(v))
}

/// `GET /.auth/<host>/callback?code=…&state=…`.
async fn callback(
    host: Host,
    State(state): State<AppState>,
    auth: Option<Extension<RequestAuth>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let name = host.name();
    if let Some(err) = &q.error {
        let detail = q.error_description.as_deref().unwrap_or(err);
        return fail(
            &state,
            &format!("{name} sign-in was not completed: {detail}"),
        );
    }
    let (Some(code), Some(flow_state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return fail(&state, &format!("Invalid {name} sign-in response."));
    };
    let cookie_ok = cookie_value(&headers, STATE_COOKIE)
        .is_some_and(|c| constant_time_eq(c.as_bytes(), flow_state.as_bytes()));
    if !cookie_ok {
        return fail(
            &state,
            "The sign-in attempt expired or came from another browser. Try again.",
        );
    }
    let accounts = state.db.external_accounts(state.keyring.clone());
    let flow = match accounts.take_flow(&hash_session_id(flow_state)).await {
        Ok(Some(f)) => f,
        Ok(None) => return fail(&state, "The sign-in attempt expired. Try again."),
        Err(e) => {
            tracing::error!(error = %e, "loading OAuth flow");
            return fail(&state, "Sign-in failed. Try again.");
        }
    };
    let Some(client_id) = flow
        .provider
        .strip_prefix(host.kind())
        .and_then(|rest| rest.strip_prefix(':'))
    else {
        return fail(&state, "The sign-in attempt expired. Try again.");
    };
    let Some(provider) = find_provider(&state, host, Some(client_id)) else {
        return fail(
            &state,
            &format!("The {name} auth provider is no longer configured."),
        );
    };
    let client = match oauth_client(&state, &provider).await {
        Ok(c) => c,
        Err(e) => return fail(&state, &e),
    };
    let identity = match identity(&client, &provider, code, &flow.pkce_verifier).await {
        Ok(i) => i,
        Err(SignInError::Denied(msg)) => return fail(&state, &msg),
        Err(SignInError::CodeHost(e)) => {
            tracing::warn!(host = host.kind(), error = %e, "code host sign-in failed");
            return fail(&state, &format!("{name} sign-in failed. Try again."));
        }
    };
    let signed_in = match state
        .auth
        .sign_in_external(
            &accounts,
            &identity,
            provider.allow_signup,
            provider.link_by_email.unwrap_or(true),
        )
        .await
    {
        Ok(s) => s,
        Err(AuthError::Internal(e)) => {
            tracing::error!(host = host.kind(), error = %e, "code host sign-in failed");
            return fail(&state, "Sign-in failed. Try again.");
        }
        Err(e) => return fail(&state, &e.to_string()),
    };
    let token = match state.auth.start_session(signed_in.user.id).await {
        Ok(t) => t,
        Err(e) => return fail(&state, &e.to_string()),
    };
    end_previous_session(&state, auth.as_deref()).await;
    queue_perms_sync(&state, signed_in.user.id).await;
    redirect(
        &flow.return_to,
        &[
            state_cookie(&state, None),
            session_set_cookie(&state, Some(token.as_str()), state.auth.session_ttl()),
        ],
    )
}

enum SignInError {
    /// Shown to the user.
    Denied(String),
    CodeHost(CodeHostError),
}

impl From<CodeHostError> for SignInError {
    fn from(e: CodeHostError) -> Self {
        Self::CodeHost(e)
    }
}

/// Exchanges the code and describes the code host user, enforcing `allowGroups` or
/// `allowOrgs`.
async fn identity(
    client: &Client,
    provider: &Provider,
    code: &str,
    verifier: &str,
) -> Result<ExternalIdentity, SignInError> {
    let host = provider.host;
    let token: OAuthToken = client.login().exchange(code, verifier).await?;
    let user = client.login().user_info(&token.access_token).await?;
    if !provider.allow.is_empty() {
        let memberships = client.memberships(&token.access_token).await?;
        let allowed = memberships.iter().any(|m| {
            provider
                .allow
                .iter()
                .any(|a| a.trim_matches('/').eq_ignore_ascii_case(m))
        });
        if !allowed {
            return Err(SignInError::Denied(format!(
                "{} user {} is not a member of {} allowed to sign in.",
                host.name(),
                user.username,
                host.membership(),
            )));
        }
    }
    let auth_data = serde_json::to_vec(&token).map_err(|e| CodeHostError::Decode(e.to_string()))?;
    Ok(ExternalIdentity {
        service_type: host.kind().to_owned(),
        service_id: client.base_url().to_string(),
        client_id: provider.client_id.clone(),
        account_id: user.account_id.clone(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        avatar_url: user.avatar_url.clone(),
        email: user.email.clone(),
        email_verified: user.email_verified,
        account_data: json!({
            "username": user.username,
            "name": user.display_name,
            "email": user.email,
            "avatarURL": user.avatar_url,
        }),
        auth_data: Some(auth_data),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_stays_on_site() {
        assert_eq!(safe_return_to(Some("/search?q=x")), "/search?q=x");
        assert_eq!(safe_return_to(Some("//evil.example")), "/");
        assert_eq!(safe_return_to(Some("https://evil.example")), "/");
        assert_eq!(safe_return_to(Some("/\\evil.example")), "/");
        assert_eq!(safe_return_to(None), "/");
    }
}
