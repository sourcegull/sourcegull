//! Opaque node IDs (Relay global object identification).
//!
//! An ID is the base64 (URL-safe alphabet, padded) encoding of `Kind:spec`, where `spec` is the
//! JSON encoding of the underlying key: `User:123` for user 123, `Site:"site"` for the singleton
//! site. Clients must treat IDs as opaque strings. Decoding also accepts the standard alphabet and
//! missing padding.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

/// Node kind of external service IDs (code host connections).
pub const EXTERNAL_SERVICE_KIND: &str = "ExternalService";

/// Encodes an integer-keyed ID, e.g. `marshal_id("User", 1)` = `VXNlcjox`.
pub fn marshal_id(kind: &str, id: i64) -> String {
    URL_SAFE.encode(format!("{kind}:{id}"))
}

/// Encodes a string-keyed ID (the key is JSON-quoted).
pub fn marshal_string_id(kind: &str, key: &str) -> String {
    let quoted = serde_json::Value::String(key.to_owned()).to_string();
    URL_SAFE.encode(format!("{kind}:{quoted}"))
}

/// Splits an ID into its kind and raw (still JSON-encoded) spec. `None` if it is not valid
/// base64 of `Kind:spec`.
pub fn unmarshal_id(id: &str) -> Option<(String, String)> {
    let bytes = [URL_SAFE, STANDARD, URL_SAFE_NO_PAD, STANDARD_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(id.trim()).ok())?;
    let text = String::from_utf8(bytes).ok()?;
    let (kind, spec) = text.split_once(':')?;
    if kind.is_empty() || spec.is_empty() {
        return None;
    }
    Some((kind.to_owned(), spec.to_owned()))
}

/// Decodes an integer-keyed ID of the given kind. `None` if the ID is malformed or of
/// another kind.
pub fn unmarshal_int_id(expected_kind: &str, id: &str) -> Option<i64> {
    let (kind, spec) = unmarshal_id(id)?;
    if kind != expected_kind {
        return None;
    }
    spec.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(marshal_id("User", 1), "VXNlcjox");
        assert_eq!(marshal_id("User", 2), "VXNlcjoy");
        let id = marshal_id("AccessToken", 12345);
        assert_eq!(unmarshal_int_id("AccessToken", &id), Some(12345));
        assert_eq!(unmarshal_int_id("User", &id), None);
        assert_eq!(unmarshal_int_id("User", "VXNlcjox"), Some(1));
        assert_eq!(unmarshal_int_id("User", "not base64!"), None);
        let site = marshal_string_id("Site", "site");
        assert_eq!(
            unmarshal_id(&site),
            Some(("Site".to_owned(), "\"site\"".to_owned()))
        );
        // Padding is optional when decoding.
        let padded = marshal_id("User", 12);
        assert!(padded.ends_with('='));
        assert_eq!(
            unmarshal_int_id("User", padded.trim_end_matches('=')),
            Some(12)
        );
    }
}
