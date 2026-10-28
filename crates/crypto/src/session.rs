//! Session identifiers. The cookie carries the id; the database stores only its hash.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// Generates a session id: 32 random bytes, base64url without padding (43 chars).
pub fn generate_session_id() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of the session id; the value stored server-side.
pub fn hash_session_id(id: &str) -> [u8; 32] {
    Sha256::digest(id.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(hash_session_id(&a), hash_session_id(&a));
        assert_ne!(hash_session_id(&a), hash_session_id(&b));
    }
}
