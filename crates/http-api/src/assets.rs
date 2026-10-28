//! The embedded web UI: static files with ETag revalidation and the SPA fallback.

use std::sync::LazyLock;

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use sourcegull_web_assets::Asset;

/// `Cache-Control` for content-hashed files.
pub const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// `Cache-Control` for everything else (`index.html`, `version.json`, ...): always revalidate.
pub const CACHE_REVALIDATE: &str = "no-cache";

/// Router fallback: serves UI files, `index.html` for client-side routes, and JSON 404s for
/// unknown API paths (`/.api/*`, `/-/*`).
pub async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if is_api_path(path) {
        return not_found();
    }
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
            Json(serde_json::json!({ "error": "method not allowed" })),
        )
            .into_response();
    }
    let asset = match sourcegull_web_assets::get(path) {
        Some(asset) => asset,
        // A missing build file must not turn into HTML (the browser would try to run it as JS).
        None if path.starts_with("/_app/") => return not_found(),
        None => sourcegull_web_assets::index_html(),
    };
    serve_asset(asset, &headers, method == Method::HEAD)
}

/// Paths served by API handlers, never by the UI.
fn is_api_path(path: &str) -> bool {
    path == "/.api" || path.starts_with("/.api/") || path == "/-" || path.starts_with("/-/")
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not found" })),
    )
        .into_response()
}

fn serve_asset(asset: Asset, req: &HeaderMap, head: bool) -> Response {
    let cache = if asset.immutable {
        CACHE_IMMUTABLE
    } else {
        CACHE_REVALIDATE
    };
    let not_modified = req
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| etag_matches(v, &asset.etag));
    let (status, body) = if not_modified {
        (StatusCode::NOT_MODIFIED, Body::empty())
    } else if head {
        (StatusCode::OK, Body::empty())
    } else {
        (StatusCode::OK, Body::from(asset.bytes))
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(etag) = HeaderValue::from_str(&asset.etag) {
        h.insert(header::ETAG, etag);
    }
    if status == StatusCode::OK {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    }
    resp
}

/// `If-None-Match` evaluation (RFC 9110 §13.1.2): weak comparison against any listed tag, or `*`.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    if_none_match.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// The `Content-Security-Policy` for every response.
///
/// SvelteKit's `index.html` bootstraps the app with an inline `<script>`; instead of allowing all
/// inline scripts, the policy allows exactly the inline scripts of the embedded `index.html` by
/// hash. Inline styles are allowed because CodeMirror injects `<style>` elements at runtime.
pub fn content_security_policy() -> &'static str {
    static CSP: LazyLock<String> = LazyLock::new(|| {
        let index = sourcegull_web_assets::index_html();
        let html = String::from_utf8_lossy(&index.bytes);
        let mut script_src = String::from("'self'");
        for script in inline_scripts(&html) {
            let hash = STANDARD.encode(Sha256::digest(script.as_bytes()));
            script_src.push_str(&format!(" 'sha256-{hash}'"));
        }
        format!(
            "default-src 'self'; script-src {script_src}; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self' data:; connect-src 'self'; \
             object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'"
        )
    });
    &CSP
}

/// The bodies of `<script>` elements without a `src` attribute.
fn inline_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        rest = &rest[start + "<script".len()..];
        let Some(tag_end) = rest.find('>') else { break };
        let attrs = &rest[..tag_end];
        rest = &rest[tag_end + 1..];
        let Some(close) = rest.find("</script>") else {
            break;
        };
        if !attrs.contains("src=") {
            out.push(&rest[..close]);
        }
        rest = &rest[close..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_comparison() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(etag_matches("W/\"abc\"", "\"abc\""));
        assert!(etag_matches("\"x\", \"abc\"", "\"abc\""));
        assert!(etag_matches("*", "\"abc\""));
        assert!(!etag_matches("\"abd\"", "\"abc\""));
    }

    #[test]
    fn finds_inline_scripts() {
        let html = r#"<head><script src="/a.js"></script></head><body><script>
  x = 1;
</script><script type="module">y()</script></body>"#;
        assert_eq!(inline_scripts(html), vec!["\n  x = 1;\n", "y()"]);
    }

    #[test]
    fn api_paths() {
        for p in ["/.api", "/.api/graphql", "/-", "/-/sign-in"] {
            assert!(is_api_path(p), "{p}");
        }
        for p in ["/", "/search", "/.apix", "/-x", "/site-admin"] {
            assert!(!is_api_path(p), "{p}");
        }
    }
}
