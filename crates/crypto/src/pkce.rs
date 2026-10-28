//! PKCE (RFC 7636) for OAuth authorization-code flows: a random verifier and its S256
//! challenge.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A code verifier: 32 random bytes, base64url without padding (43 chars, the RFC minimum).
pub fn generate_verifier() -> String {
    crate::session::generate_session_id()
}

/// The `S256` code challenge of `verifier`: base64url(SHA-256(verifier)) without padding.
pub fn challenge_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b() {
        assert_eq!(
            challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(generate_verifier().len(), 43);
    }
}
