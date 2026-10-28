//! OpenID Connect sign-in, using the shared single-use OAuth flow store.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::{Value, json};
use sourcegull_authn::{AuthError, ExternalIdentity};
use sourcegull_config::{AuthProvider, OpenIdConnectAuthProvider};
use sourcegull_crypto::constant_time_eq;
use sourcegull_crypto::pkce;
use sourcegull_crypto::session::{generate_session_id, hash_session_id};
use sourcegull_db::OAuthFlow;
use sourcegull_http_extract::RequestAuth;
use sourcegull_services::AppState;

use crate::auth_routes::{end_previous_session, queue_perms_sync, session_set_cookie};
use crate::oauth_routes::{self, FLOW_TTL, STATE_COOKIE};

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
static DISCOVERY: OnceLock<Mutex<HashMap<String, (Instant, Discovery)>>> = OnceLock::new();

#[derive(Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    userinfo_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct LoginQuery {
    pc: Option<String>,
    #[serde(rename = "returnTo")]
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub fn oidc_routes() -> Router<AppState> {
    Router::new()
        .route("/.auth/openidconnect/login", get(login))
        .route("/.auth/openidconnect/callback", get(callback))
}

fn provider_id(p: &OpenIdConnectAuthProvider) -> &str {
    p.config_id.as_deref().unwrap_or(&p.client_id)
}

pub fn provider_button(p: &OpenIdConnectAuthProvider) -> Value {
    json!({
        "type": "openidconnect",
        "displayName": p.display_name.as_deref().unwrap_or("OpenID Connect"),
        "authenticationURL": format!("/.auth/openidconnect/login?pc={}",
            percent_encoding::utf8_percent_encode(provider_id(p), percent_encoding::NON_ALPHANUMERIC)),
    })
}

fn find_provider(state: &AppState, id: Option<&str>) -> Option<OpenIdConnectAuthProvider> {
    let config = state.config.load();
    let mut providers = config.value.auth_providers.iter().filter_map(|p| match p {
        AuthProvider::OpenIdConnect(p) => Some(p),
        _ => None,
    });
    match id {
        Some(id) => providers.find(|p| provider_id(p) == id).cloned(),
        None => {
            let first = providers.next()?;
            providers.next().is_none().then(|| first.clone())
        }
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

async fn discovery(client: &reqwest::Client, issuer: &str) -> Result<Discovery, String> {
    let cache = DISCOVERY.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((at, doc)) = cache.lock().unwrap().get(issuer)
        && at.elapsed() < DISCOVERY_TTL
    {
        return Ok(doc.clone());
    }
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let doc: Discovery = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if doc.issuer != issuer {
        return Err("OIDC discovery issuer mismatch".into());
    }
    for endpoint in [
        &doc.authorization_endpoint,
        &doc.token_endpoint,
        &doc.jwks_uri,
    ] {
        let parsed = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
        if parsed.scheme() != "https" && parsed.scheme() != "http" {
            return Err("Invalid OIDC endpoint".into());
        }
    }
    cache
        .lock()
        .unwrap()
        .insert(issuer.to_owned(), (Instant::now(), doc.clone()));
    Ok(doc)
}

async fn login(State(state): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    let Some(provider) = find_provider(&state, q.pc.as_deref()) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Unknown OpenID Connect auth provider.", "code": "NOT_FOUND"})),
        )
            .into_response();
    };
    let Some(external) = state.config.load().value.external_url.clone() else {
        return oauth_routes::fail(&state, "OpenID Connect sign-in needs externalURL.");
    };
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return oauth_routes::fail(&state, &e),
    };
    let doc = match discovery(&client, &provider.issuer).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC discovery failed");
            return oauth_routes::fail(&state, "OpenID Connect sign-in is unavailable.");
        }
    };
    let flow_state = generate_session_id();
    let nonce = generate_session_id();
    let verifier = pkce::generate_verifier();
    let mut url = match reqwest::Url::parse(&doc.authorization_endpoint) {
        Ok(u) => u,
        Err(_) => return oauth_routes::fail(&state, "Invalid OIDC authorization endpoint."),
    };
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &provider.client_id)
        .append_pair(
            "redirect_uri",
            &format!(
                "{}/.auth/openidconnect/callback",
                external.trim_end_matches('/')
            ),
        )
        .append_pair("scope", "openid email profile")
        .append_pair("state", &flow_state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &pkce::challenge_s256(&verifier))
        .append_pair("code_challenge_method", "S256");
    let flow = OAuthFlow {
        provider: format!("openidconnect:{}", provider_id(&provider)),
        pkce_verifier: verifier,
        return_to: oauth_routes::safe_return_to(q.return_to.as_deref()),
        nonce: Some(nonce),
    };
    let expires = chrono::Utc::now() + chrono::Duration::from_std(FLOW_TTL).unwrap_or_default();
    if let Err(e) = state
        .db
        .external_accounts(state.keyring.clone())
        .create_flow(&hash_session_id(&flow_state), &flow, expires)
        .await
    {
        tracing::error!(error = %e, "storing OIDC flow");
        return oauth_routes::fail(&state, "Sign-in failed. Try again.");
    }
    oauth_routes::redirect(
        url.as_str(),
        &[oauth_routes::state_cookie(&state, Some(&flow_state))],
    )
}

async fn callback(
    State(state): State<AppState>,
    auth: Option<Extension<RequestAuth>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if q.error.is_some() {
        return oauth_routes::fail(&state, "OpenID Connect sign-in was not completed.");
    }
    let (Some(code), Some(flow_state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return oauth_routes::fail(&state, "Invalid OpenID Connect sign-in response.");
    };
    let cookie_ok = oauth_routes::cookie_value(&headers, STATE_COOKIE)
        .is_some_and(|c| constant_time_eq(c.as_bytes(), flow_state.as_bytes()));
    if !cookie_ok {
        return oauth_routes::fail(
            &state,
            "The sign-in attempt expired or came from another browser. Try again.",
        );
    }
    let accounts = state.db.external_accounts(state.keyring.clone());
    let flow = match accounts.take_flow(&hash_session_id(flow_state)).await {
        Ok(Some(f)) => f,
        Ok(None) => return oauth_routes::fail(&state, "The sign-in attempt expired. Try again."),
        Err(e) => {
            tracing::error!(error = %e, "loading OIDC flow");
            return oauth_routes::fail(&state, "Sign-in failed. Try again.");
        }
    };
    let Some(id) = flow.provider.strip_prefix("openidconnect:") else {
        return oauth_routes::fail(&state, "Invalid OpenID Connect sign-in response.");
    };
    let Some(provider) = find_provider(&state, Some(id)) else {
        return oauth_routes::fail(
            &state,
            "The OpenID Connect provider is no longer configured.",
        );
    };
    let identity = match oidc_identity(&state, &provider, &flow, code).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC sign-in failed");
            return oauth_routes::fail(&state, &e);
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
            tracing::error!(error = %e, "OIDC sign-in failed");
            return oauth_routes::fail(&state, "Sign-in failed. Try again.");
        }
        Err(e) => return oauth_routes::fail(&state, &e.to_string()),
    };
    let token = match state.auth.start_session(signed_in.user.id).await {
        Ok(t) => t,
        Err(e) => return oauth_routes::fail(&state, &e.to_string()),
    };
    end_previous_session(&state, auth.as_deref()).await;
    queue_perms_sync(&state, signed_in.user.id).await;
    oauth_routes::redirect(
        &flow.return_to,
        &[
            oauth_routes::state_cookie(&state, None),
            session_set_cookie(&state, Some(token.as_str()), state.auth.session_ttl()),
        ],
    )
}

async fn oidc_identity(
    state: &AppState,
    provider: &OpenIdConnectAuthProvider,
    flow: &OAuthFlow,
    code: &str,
) -> Result<ExternalIdentity, String> {
    let client = http_client()?;
    let doc = discovery(&client, &provider.issuer).await?;
    let external = state
        .config
        .load()
        .value
        .external_url
        .clone()
        .ok_or("Missing externalURL")?;
    let tokens: TokenResponse = client
        .post(&doc.token_endpoint)
        .basic_auth(&provider.client_id, Some(&provider.client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            (
                "redirect_uri",
                &format!(
                    "{}/.auth/openidconnect/callback",
                    external.trim_end_matches('/')
                ),
            ),
            ("code_verifier", &flow.pkce_verifier),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let header = decode_header(&tokens.id_token).map_err(|e| e.to_string())?;
    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    ) {
        return Err("Unsupported ID token signing algorithm.".into());
    }
    let jwks: Value = client
        .get(&doc.jwks_uri)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let keys = jwks["keys"].as_array().ok_or("Invalid JWKS")?;
    let key = keys
        .iter()
        .find(|k| {
            matches!(k["kty"].as_str(), Some("RSA" | "EC" | "OKP"))
                && k["use"].as_str().is_none_or(|u| u == "sig")
                && k["alg"]
                    .as_str()
                    .is_none_or(|a| a == format!("{:?}", header.alg))
                && (header.kid.is_none() || k["kid"].as_str() == header.kid.as_deref())
        })
        .ok_or("No matching ID token signing key")?;
    let jwk = serde_json::from_value(key.clone()).map_err(|e| e.to_string())?;
    let decoding_key = DecodingKey::from_jwk(&jwk).map_err(|e| e.to_string())?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&doc.issuer]);
    validation.set_audience(&[&provider.client_id]);
    validation
        .required_spec_claims
        .extend(["iss", "aud", "sub", "nonce"].map(str::to_owned));
    let claims = decode::<Value>(&tokens.id_token, &decoding_key, &validation)
        .map_err(|e| format!("Invalid ID token: {e}"))?
        .claims;
    if (claims["aud"].as_array().is_some_and(|a| a.len() > 1) || !claims["azp"].is_null())
        && claims["azp"].as_str() != Some(provider.client_id.as_str())
    {
        return Err("Invalid ID token authorized party.".into());
    }
    let nonce = flow.nonce.as_deref().ok_or("Missing OIDC nonce")?;
    if !claims["nonce"]
        .as_str()
        .is_some_and(|v| constant_time_eq(v.as_bytes(), nonce.as_bytes()))
    {
        return Err("Invalid ID token nonce.".into());
    }
    let sub = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Missing ID token subject")?;
    let mut profile = claims.clone();
    let groups_claim = provider.groups_claim.as_deref().unwrap_or("groups");
    let needs_userinfo_email = profile["email"].is_null() || profile["email_verified"].is_null();
    if (needs_userinfo_email
        || (!provider.allow_groups.is_empty()
            && profile.get(groups_claim).is_none_or(Value::is_null)))
        && let (Some(endpoint), Some(access_token)) = (&doc.userinfo_endpoint, &tokens.access_token)
    {
        let userinfo: Value = client
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if userinfo["sub"].as_str() != Some(sub) {
            return Err("Userinfo subject mismatch.".into());
        }
        if needs_userinfo_email {
            profile["email"] = userinfo["email"].clone();
            profile["email_verified"] = userinfo["email_verified"].clone();
        }
        for field in ["preferred_username", "name", "picture"] {
            if profile[field].is_null() {
                profile[field] = userinfo[field].clone();
            }
        }
        if profile.get(groups_claim).is_none_or(Value::is_null) {
            profile[groups_claim] = userinfo[groups_claim].clone();
        }
    }
    if profile["email_verified"] != true {
        return Err("A verified email is required to sign in.".into());
    }
    let email = profile["email"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("A verified email is required to sign in.")?;
    sourcegull_authn::validate::validate_email(email)
        .map_err(|_| "A valid verified email is required to sign in.")?;
    if let Some(domain) = &provider.require_email_domain
        && !email
            .rsplit_once('@')
            .is_some_and(|(_, d)| d.eq_ignore_ascii_case(domain.trim_start_matches('@')))
    {
        return Err("Email domain is not allowed to sign in.".into());
    }
    if !provider.allow_groups.is_empty() {
        let allowed = profile
            .get(groups_claim)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|group| provider.allow_groups.iter().any(|a| a == group))
            });
        if !allowed {
            return Err("User is not a member of a group allowed to sign in.".into());
        }
    }
    let username = profile["preferred_username"]
        .as_str()
        .filter(|s| sourcegull_authn::validate::validate_username(s).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("oidc-{}", hex_subject(sub)));
    Ok(ExternalIdentity {
        service_type: "openidconnect".into(),
        service_id: doc.issuer,
        client_id: provider.client_id.clone(),
        account_id: sub.to_owned(),
        username,
        display_name: profile["name"].as_str().map(str::to_owned),
        avatar_url: profile["picture"].as_str().map(str::to_owned),
        email: Some(email.to_owned()),
        email_verified: true,
        account_data: json!({ "sub": sub, "email": email, "name": profile["name"] }),
        auth_data: None,
    })
}

fn hex_subject(subject: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(subject.as_bytes())[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
