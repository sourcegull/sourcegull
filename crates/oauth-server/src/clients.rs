//! OAuth clients: Dynamic Client Registration (RFC 7591), client ID metadata documents, and
//! the redirect URI rules both follow.
//!
//! Every client is public (`token_endpoint_auth_method: none`) and proves possession of its
//! authorization codes with PKCE. A client registering with another authentication method is
//! registered as public; the response says so.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sourcegull_db::{NewOAuthClient, OAuthClient, OAuthClientKind};
use sourcegull_http_extract::ClientIp;
use sourcegull_outbound_webhooks::guard::{self, Allowlist, Lookup, LookupFuture, SystemLookup};
use sourcegull_services::AppState;
use url::{Host, Url};

use crate::{not_found, oauth_error, public_json_status, server_error};

/// Most redirect URIs a client may have.
pub const MAX_REDIRECT_URIS: usize = 10;
/// Longest redirect URI accepted.
const MAX_URI_LEN: usize = 2000;
/// Longest client name kept.
const MAX_NAME_LEN: usize = 200;
/// Registrations allowed per client IP address per hour.
pub const REGISTRATIONS_PER_IP_PER_HOUR: i64 = 60;
/// Registrations allowed per hour in total.
pub const REGISTRATIONS_PER_HOUR: i64 = 1000;
/// Registered clients that never got a grant are deleted after this long.
pub const UNUSED_CLIENT_TTL: chrono::Duration = chrono::Duration::days(30);
/// Metadata document clients without a live grant are deleted this long after their last
/// fetch.
pub const UNUSED_METADATA_CLIENT_TTL: chrono::Duration = chrono::Duration::days(1);
/// How long a fetched client ID metadata document is used before it is fetched again.
pub const METADATA_DOCUMENT_TTL: chrono::Duration = chrono::Duration::hours(1);
/// Timeout of a metadata document fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest metadata document accepted.
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

/// Whether `uri` may be a redirect URI: an `http` loopback URI (`127.0.0.1`, `[::1]` or
/// `localhost`, any port) or an `https` URI whose host is in `allowed_hosts` (any host when
/// it is empty; `*.example.com` matches subdomains), without a fragment or credentials.
/// Returns why not.
pub fn check_redirect_uri(uri: &str, allowed_hosts: &[String]) -> Result<Url, String> {
    if uri.len() > MAX_URI_LEN {
        return Err("redirect URI is too long".into());
    }
    let url = Url::parse(uri).map_err(|_| format!("{uri:?} is not an absolute URL"))?;
    if url.fragment().is_some() {
        return Err(format!("redirect URI {uri:?} must not have a fragment"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("redirect URI {uri:?} must not contain credentials"));
    }
    match url.scheme() {
        "http" if is_loopback(&url) => Ok(url),
        "http" => Err(format!(
            "redirect URI {uri:?} must use https, or http on a loopback address"
        )),
        "https" => {
            let host = url.host_str().unwrap_or_default();
            if allowed_hosts.is_empty() || host_allowed(host, allowed_hosts) {
                Ok(url)
            } else {
                Err(format!(
                    "redirect URI host {host:?} is not in mcp.oauth.allowedRedirectHosts"
                ))
            }
        }
        other => Err(format!(
            "redirect URI scheme {other:?} is not allowed: use https, or http on a loopback address"
        )),
    }
}

/// Whether the URL's host is a loopback address or `localhost`.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowed.iter().any(|entry| {
        let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
            None => host == entry,
        }
    })
}

/// Whether the redirect URI a request names matches one the client registered: exactly, or
/// for loopback URIs with any port (RFC 8252 §7.3).
pub fn redirect_uri_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    let (Ok(a), Ok(b)) = (Url::parse(registered), Url::parse(requested)) else {
        return false;
    };
    a.scheme() == "http"
        && b.scheme() == "http"
        && is_loopback(&a)
        && a.host() == b.host()
        && a.path() == b.path()
        && a.query() == b.query()
        && b.fragment().is_none()
}

/// Whether `client_id` is a client ID metadata document URL: `https` with a path, without a
/// fragment or credentials.
pub fn is_metadata_document_url(client_id: &str) -> bool {
    Url::parse(client_id).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host().is_some()
            && u.path() != "/"
            && u.fragment().is_none()
            && u.username().is_empty()
            && u.password().is_none()
    })
}

fn clean_name(name: Option<&str>) -> String {
    name.unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A `client_uri` worth showing: an `https` URL.
fn clean_client_uri(uri: Option<&str>) -> Option<String> {
    let uri = uri?.trim();
    (uri.len() <= MAX_URI_LEN
        && Url::parse(uri).is_ok_and(|u| u.scheme() == "https" && u.host().is_some()))
    .then(|| uri.to_owned())
}

fn check_redirect_uris(uris: &[String], allowed: &[String]) -> Result<(), String> {
    if uris.is_empty() {
        return Err("redirect_uris must list at least one redirect URI".into());
    }
    if uris.len() > MAX_REDIRECT_URIS {
        return Err(format!(
            "redirect_uris may list at most {MAX_REDIRECT_URIS} redirect URIs"
        ));
    }
    for uri in uris {
        check_redirect_uri(uri, allowed)?;
    }
    Ok(())
}

/// Client metadata, as registered or as a metadata document holds it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ClientMetadata {
    /// The document's own URL (metadata documents only).
    pub client_id: Option<String>,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub client_uri: Option<String>,
    pub grant_types: Option<Vec<String>>,
    pub response_types: Option<Vec<String>>,
    pub token_endpoint_auth_method: Option<String>,
    pub scope: Option<String>,
}

impl ClientMetadata {
    /// Checks what both kinds of clients share: redirect URIs, grant and response types.
    fn check(&self, allowed: &[String]) -> Result<(), String> {
        check_redirect_uris(&self.redirect_uris, allowed)?;
        if let Some(grants) = &self.grant_types
            && let Some(bad) = grants
                .iter()
                .find(|g| !matches!(g.as_str(), "authorization_code" | "refresh_token"))
        {
            return Err(format!("grant type {bad:?} is not supported"));
        }
        if let Some(types) = &self.response_types
            && let Some(bad) = types.iter().find(|t| t.as_str() != "code")
        {
            return Err(format!("response type {bad:?} is not supported"));
        }
        Ok(())
    }
}

/// Checks a fetched client ID metadata document of `url`: it names itself as `client_id` and
/// uses no client secret.
pub fn parse_metadata_document(
    url: &str,
    body: &[u8],
    allowed: &[String],
) -> Result<ClientMetadata, String> {
    // The parser's message may quote the document; it is not shown.
    let doc: ClientMetadata = serde_json::from_slice(body)
        .map_err(|_| "the client metadata document is not valid client metadata".to_owned())?;
    if doc.client_id.as_deref() != Some(url) {
        return Err("the client metadata document's client_id is not its URL".into());
    }
    if let Some(method) = doc.token_endpoint_auth_method.as_deref()
        && method != "none"
    {
        return Err(format!(
            "token_endpoint_auth_method {method:?} is not supported: clients are public"
        ));
    }
    doc.check(allowed)?;
    Ok(doc)
}

/// Why a client could not be resolved.
#[derive(Debug)]
pub enum ClientError {
    /// No such client, or its metadata is not acceptable.
    Invalid(String),
    /// The client's metadata document must be fetched, and only a signed-in user's request
    /// fetches one.
    SignInRequired,
    /// A database error.
    Internal(sourcegull_db::DbError),
}

impl From<sourcegull_db::DbError> for ClientError {
    fn from(e: sourcegull_db::DbError) -> Self {
        Self::Internal(e)
    }
}

/// The stored client with `client_id`, as the token and revocation endpoints know it: a
/// registered client, or a metadata document client fetched when a user authorized it.
pub async fn stored(state: &AppState, client_id: &str) -> Result<OAuthClient, ClientError> {
    state
        .db
        .oauth()
        .client(client_id)
        .await?
        .ok_or_else(|| ClientError::Invalid(format!("Unknown client {client_id:?}.")))
}

/// The client with `client_id` for an authorization request: a stored registered client, or
/// the client of the metadata document at that URL, fetched when not cached or stale. Only a
/// request of a signed-in user (`signed_in`) fetches a document.
pub async fn resolve(
    state: &AppState,
    client_id: &str,
    signed_in: bool,
) -> Result<OAuthClient, ClientError> {
    let store = state.db.oauth();
    let cached = store.client(client_id).await?;
    if let Some(client) = &cached
        && (client.kind == OAuthClientKind::Registered
            || client
                .fetched_at
                .is_some_and(|t| t > Utc::now() - METADATA_DOCUMENT_TTL))
    {
        return Ok(client.clone());
    }
    if !is_metadata_document_url(client_id) {
        return Err(ClientError::Invalid(format!(
            "Unknown client {client_id:?}."
        )));
    }
    if !signed_in {
        return Err(ClientError::SignInRequired);
    }
    let doc = fetch_metadata_document(state, client_id)
        .await
        .map_err(ClientError::Invalid)?;
    let client = store
        .upsert_metadata_client(&NewOAuthClient {
            client_id,
            client_name: &clean_name(doc.client_name.as_deref()),
            client_uri: clean_client_uri(doc.client_uri.as_deref()).as_deref(),
            redirect_uris: &doc.redirect_uris,
        })
        .await?;
    Ok(client)
}

/// What the user sees when a metadata document cannot be fetched; the reason is logged.
pub const FETCH_FAILED: &str = "The app's client metadata document could not be fetched.";

/// Resolves host names for metadata document fetches: fails for a host name in
/// `webhooks.outbound.allowedHosts` or one resolving to an address in it, then checks the
/// addresses like [`SystemLookup`] with no exemptions.
struct DeniedHosts(Allowlist);

impl Lookup for DeniedHosts {
    fn lookup(&self, host: &str) -> LookupFuture {
        let denied = self.0.allows_host(host);
        let allow = self.0.clone();
        let lookup = SystemLookup.lookup(host);
        Box::pin(async move {
            if denied {
                return Err(std::io::Error::other(
                    "host is in webhooks.outbound.allowedHosts",
                ));
            }
            let ips = lookup.await?;
            if ips.iter().any(|ip| allow.allows_ip(*ip)) {
                return Err(std::io::Error::other(
                    "address is in webhooks.outbound.allowedHosts",
                ));
            }
            Ok(ips)
        })
    }
}

/// Whether the URL's host is listed in `webhooks.outbound.allowedHosts`: such hosts are
/// internal by intent, so documents are never fetched from them.
fn internal_host(url: &Url, internal: &Allowlist) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => internal.allows_ip(ip.into()),
        Some(Host::Ipv6(ip)) => internal.allows_ip(ip.into()),
        Some(Host::Domain(d)) => internal.allows_host(d),
        None => true,
    }
}

async fn fetch_metadata_document(state: &AppState, url: &str) -> Result<ClientMetadata, String> {
    let config = state.config.load();
    let fail = |reason: &dyn std::fmt::Display| {
        tracing::warn!(url, reason = %reason, "fetching an OAuth client metadata document");
        FETCH_FAILED.to_owned()
    };
    let internal = Allowlist::new(&config.value.webhooks_outbound_allowed_hosts);
    let parsed = Url::parse(url).map_err(|e| fail(&e))?;
    if internal_host(&parsed, &internal) {
        return Err(fail(&"the host is in webhooks.outbound.allowedHosts"));
    }
    // No exemptions: private addresses are refused whatever webhooks may reach.
    let allow = Arc::new(Allowlist::default());
    guard::check_url(&parsed, &allow).map_err(|e| fail(&e))?;
    let client = guard::client(allow, Arc::new(DeniedHosts(internal)), FETCH_TIMEOUT)
        .map_err(|e| fail(&e))?;
    let mut resp = client
        .get(parsed)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| fail(&e))?;
    if !resp.status().is_success() {
        return Err(fail(&format_args!("status {}", resp.status())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| fail(&e))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_DOCUMENT_BYTES {
            return Err(fail(&"the document is too large"));
        }
    }
    parse_metadata_document(url, &body, &config.value.mcp_oauth_allowed_redirect_hosts)
}

/// `POST /.oauth/register`: Dynamic Client Registration.
pub async fn register(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    body: Bytes,
) -> Response {
    let config = state.config.load();
    if !config.value.mcp_oauth_enabled || !config.value.mcp_oauth_dynamic_client_registration {
        return not_found();
    }
    let invalid = |msg: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata", msg);
    let meta: ClientMetadata = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => return invalid(&format!("Invalid client metadata: {e}")),
    };
    if meta.client_id.is_some() {
        return invalid("client_id is assigned by the server.");
    }
    let allowed = &config.value.mcp_oauth_allowed_redirect_hosts;
    if let Err(e) = check_redirect_uris(&meta.redirect_uris, allowed) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &e);
    }
    if let Err(e) = meta.check(allowed) {
        return invalid(&e);
    }
    let store = state.db.oauth();
    let ip = ip.map(|ip| ip.to_string());
    let now = Utc::now();
    match store
        .registrations_since(now - chrono::Duration::hours(1), ip.as_deref())
        .await
    {
        Ok((from_ip, total))
            if from_ip >= REGISTRATIONS_PER_IP_PER_HOUR || total >= REGISTRATIONS_PER_HOUR =>
        {
            return oauth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "invalid_request",
                "Too many client registrations; try again later.",
            );
        }
        Ok(_) => {}
        Err(e) => return server_error("counting OAuth client registrations", e),
    }
    let client_id = format!(
        "sourcegull-{}",
        sourcegull_crypto::session::generate_session_id()
    );
    let name = clean_name(meta.client_name.as_deref());
    let client_uri = clean_client_uri(meta.client_uri.as_deref());
    let client = match store
        .register_client(
            &NewOAuthClient {
                client_id: &client_id,
                client_name: &name,
                client_uri: client_uri.as_deref(),
                redirect_uris: &meta.redirect_uris,
            },
            ip.as_deref(),
        )
        .await
    {
        Ok(c) => c,
        Err(e) => return server_error("registering an OAuth client", e),
    };
    let grant_types = meta
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".into(), "refresh_token".into()]);
    let mut out = json!({
        "client_id": client.client_id,
        "client_id_issued_at": client.created_at.timestamp(),
        "redirect_uris": client.redirect_uris,
        "grant_types": grant_types,
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if !client.client_name.is_empty() {
        out["client_name"] = json!(client.client_name);
    }
    if let Some(uri) = client.client_uri {
        out["client_uri"] = json!(uri);
    }
    if let Some(scope) = meta.scope {
        out["scope"] = json!(scope);
    }
    public_json_status(StatusCode::CREATED, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_rules() {
        let none: &[String] = &[];
        for ok in [
            "http://127.0.0.1:3333/callback",
            "http://127.0.0.1/cb",
            "http://[::1]:8080/cb",
            "http://localhost:6274/oauth/callback",
            "https://app.example.com/cb?x=1",
        ] {
            assert!(check_redirect_uri(ok, none).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com/cb",
            "http://10.0.0.1/cb",
            "https://app.example.com/cb#frag",
            "https://user:pw@app.example.com/cb",
            "javascript:alert(1)",
            "myapp://callback",
            "/relative",
        ] {
            assert!(check_redirect_uri(bad, none).is_err(), "{bad}");
        }
        let allowed = vec!["vscode.dev".to_owned(), "*.example.com".to_owned()];
        assert!(check_redirect_uri("https://vscode.dev/redirect", &allowed).is_ok());
        assert!(check_redirect_uri("https://a.example.com/cb", &allowed).is_ok());
        assert!(check_redirect_uri("https://example.com/cb", &allowed).is_err());
        assert!(check_redirect_uri("https://evil.dev/cb", &allowed).is_err());
        assert!(check_redirect_uri("https://notexample.com/cb", &allowed).is_err());
        assert!(check_redirect_uri("http://127.0.0.1:1/cb", &allowed).is_ok());
    }

    #[test]
    fn loopback_redirects_match_any_port() {
        assert!(redirect_uri_matches(
            "http://127.0.0.1:3000/cb",
            "http://127.0.0.1:51234/cb"
        ));
        assert!(redirect_uri_matches(
            "http://localhost/cb",
            "http://localhost:8080/cb"
        ));
        assert!(!redirect_uri_matches(
            "http://127.0.0.1:3000/cb",
            "http://127.0.0.1:3000/other"
        ));
        assert!(!redirect_uri_matches(
            "http://127.0.0.1:3000/cb",
            "http://localhost:3000/cb"
        ));
        assert!(!redirect_uri_matches(
            "https://a.example.com/cb",
            "https://a.example.com:444/cb"
        ));
        assert!(redirect_uri_matches(
            "https://a.example.com/cb",
            "https://a.example.com/cb"
        ));
    }

    #[test]
    fn metadata_documents() {
        let url = "https://client.example.com/oauth/client.json";
        assert!(is_metadata_document_url(url));
        assert!(!is_metadata_document_url("https://client.example.com/"));
        assert!(!is_metadata_document_url(
            "http://client.example.com/c.json"
        ));
        assert!(!is_metadata_document_url("sourcegull-abc"));
        let doc = format!(
            r#"{{"client_id": "{url}", "client_name": "Agent", "redirect_uris": ["http://127.0.0.1:3000/cb"]}}"#
        );
        let parsed = parse_metadata_document(url, doc.as_bytes(), &[]).unwrap();
        assert_eq!(parsed.client_name.as_deref(), Some("Agent"));
        let other = r#"{"client_id": "https://other.example.com/c.json", "redirect_uris": ["http://127.0.0.1/cb"]}"#;
        assert!(parse_metadata_document(url, other.as_bytes(), &[]).is_err());
        let secret = format!(
            r#"{{"client_id": "{url}", "token_endpoint_auth_method": "client_secret_basic", "redirect_uris": ["http://127.0.0.1/cb"]}}"#
        );
        assert!(parse_metadata_document(url, secret.as_bytes(), &[]).is_err());
        let no_redirects = format!(r#"{{"client_id": "{url}"}}"#);
        assert!(parse_metadata_document(url, no_redirects.as_bytes(), &[]).is_err());
        let implicit = format!(
            r#"{{"client_id": "{url}", "grant_types": ["implicit"], "redirect_uris": ["http://127.0.0.1/cb"]}}"#
        );
        assert!(parse_metadata_document(url, implicit.as_bytes(), &[]).is_err());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name(Some("  My\u{7}Agent  ")), "MyAgent");
        assert_eq!(clean_name(None), "");
        assert_eq!(clean_name(Some(&"x".repeat(500))).len(), MAX_NAME_LEN);
        assert_eq!(clean_client_uri(Some("javascript:x")), None);
        assert_eq!(
            clean_client_uri(Some("https://agent.example.com")).as_deref(),
            Some("https://agent.example.com")
        );
    }
}
