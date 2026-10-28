//! The web UI (`ui/build`, a SvelteKit SPA) embedded into the binary.
//!
//! The HTTP layer serves [`get`] for asset paths and falls back to [`index_html`] for every other
//! UI route, since routing happens client-side. When the binary was built without the UI, a small
//! placeholder page is served instead.
//!
//! In debug builds `rust-embed` reads files from disk at runtime, so `npm run build` is picked up
//! without recompiling.

use std::borrow::Cow;

use rust_embed::{EmbeddedFile, RustEmbed};

#[derive(RustEmbed)]
#[folder = "$SOURCEGULL_UI_ASSETS_DIR"]
struct UiBuild;

/// Path prefix of content-hashed files that never change for a given URL.
const IMMUTABLE_PREFIX: &str = "_app/immutable/";

const PLACEHOLDER_HTML: &[u8] = include_bytes!("../placeholder/index.html");

/// A static file of the web UI, ready to be served.
#[derive(Debug, Clone)]
pub struct Asset {
    /// File contents.
    pub bytes: Cow<'static, [u8]>,
    /// `Content-Type` header value.
    pub mime: &'static str,
    /// Strong `ETag` header value, quoted (derived from the content hash).
    pub etag: String,
    /// Whether the file is content-hashed and can be cached forever
    /// (`Cache-Control: public, max-age=31536000, immutable`).
    pub immutable: bool,
}

/// Whether the real UI was embedded at build time (false means the placeholder page is served).
pub fn ui_embedded() -> bool {
    env!("SOURCEGULL_UI_EMBEDDED") == "true"
}

/// Looks up a UI file by URL path (leading `/` optional, no query string).
///
/// Returns `None` for unknown files, directories and paths with `..` or empty segments; the caller
/// should then serve [`index_html`] (for UI routes) or a 404.
pub fn get(path: &str) -> Option<Asset> {
    let path = normalize(path)?;
    let file = UiBuild::get(path)?;
    Some(to_asset(path, file))
}

/// The SPA entry page, served for every UI route that is not a file.
pub fn index_html() -> Asset {
    match UiBuild::get("index.html") {
        Some(file) => to_asset("index.html", file),
        None => Asset {
            bytes: Cow::Borrowed(PLACEHOLDER_HTML),
            mime: HTML,
            etag: format!("\"placeholder-{}\"", env!("CARGO_PKG_VERSION")),
            immutable: false,
        },
    }
}

fn normalize(path: &str) -> Option<&str> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let valid = !path.is_empty()
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.contains('\\'));
    valid.then_some(path)
}

fn to_asset(path: &str, file: EmbeddedFile) -> Asset {
    let hash = file.metadata.sha256_hash();
    Asset {
        etag: format!("\"{}\"", hex::encode(&hash[..16])),
        mime: mime_for(path),
        immutable: path.starts_with(IMMUTABLE_PREFIX),
        bytes: file.data,
    }
}

const HTML: &str = "text/html; charset=utf-8";

/// `Content-Type` for a path, with an explicit charset for text formats.
fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => HTML,
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        _ => mime_guess::from_path(path)
            .first_raw()
            .unwrap_or("application/octet-stream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_paths() {
        for p in [
            "",
            "/",
            "../Cargo.toml",
            "/_app/../index.html",
            "a//b",
            "./index.html",
            "a\\b",
        ] {
            assert!(get(p).is_none(), "{p:?} should not resolve");
        }
        assert_eq!(normalize("/_app/version.json"), Some("_app/version.json"));
    }

    #[test]
    fn unknown_file_is_none() {
        assert!(get("/definitely-not-a-file.js").is_none());
    }

    #[test]
    fn index_html_always_available() {
        let index = index_html();
        assert_eq!(index.mime, "text/html; charset=utf-8");
        assert!(!index.immutable);
        assert!(index.etag.starts_with('"') && index.etag.ends_with('"'));
        let html = String::from_utf8_lossy(&index.bytes);
        assert!(html.contains("<html"), "{html}");
        if !ui_embedded() {
            assert!(html.contains("built without the web UI"));
        }
    }

    #[test]
    fn serves_built_files() {
        if !ui_embedded() {
            return;
        }
        let index = get("/index.html").expect("index.html");
        assert_eq!(index.etag, index_html().etag);
        let version = get("_app/version.json").expect("version.json");
        assert_eq!(version.mime, "application/json");
        assert!(!version.immutable);
        let immutable = UiBuild::iter()
            .find(|p| p.starts_with(IMMUTABLE_PREFIX) && p.ends_with(".js"))
            .expect("an immutable JS chunk");
        let asset = get(&format!("/{immutable}")).expect("chunk");
        assert!(asset.immutable);
        assert_eq!(asset.mime, "text/javascript; charset=utf-8");
        assert_eq!(asset.etag.len(), 34);
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_for("a/b.CSS"), "text/css; charset=utf-8");
        assert_eq!(mime_for("favicon.svg"), "image/svg+xml");
        assert_eq!(mime_for("logo.png"), "image/png");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }
}
