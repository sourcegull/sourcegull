//! Password hashing with argon2id, stored as PHC strings (`$argon2id$v=19$...`).

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use rand::rngs::OsRng;

/// Errors from password hashing.
#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    /// The cost parameters are out of range.
    #[error("invalid argon2 parameters: {0}")]
    Params(String),
    /// The stored hash is not a valid PHC string.
    #[error("invalid password hash: {0}")]
    InvalidHash(String),
    /// Hashing failed.
    #[error("hashing password: {0}")]
    Hash(String),
}

/// argon2id cost parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordParams {
    /// Memory cost in KiB.
    pub memory_kib: u32,
    /// Number of passes.
    pub iterations: u32,
    /// Degree of parallelism.
    pub parallelism: u32,
}

impl PasswordParams {
    /// OWASP Password Storage Cheat Sheet recommendation: m = 19 MiB, t = 2, p = 1.
    pub const OWASP: Self = Self {
        memory_kib: 19 * 1024,
        iterations: 2,
        parallelism: 1,
    };

    /// Minimal cost, for tests only.
    pub const TEST: Self = Self {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };
}

impl Default for PasswordParams {
    fn default() -> Self {
        Self::OWASP
    }
}

/// Hashes `password` with a random salt; returns a PHC string that embeds the parameters.
pub fn hash_password(password: &str, params: &PasswordParams) -> Result<String, PasswordError> {
    let params = Params::new(
        params.memory_kib,
        params.iterations,
        params.parallelism,
        None,
    )
    .map_err(|e| PasswordError::Params(e.to_string()))?;
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| PasswordError::Hash(e.to_string()))?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Verifies `password` against a PHC string from [`hash_password`], using the
/// parameters recorded in it. Returns `Ok(false)` on mismatch.
pub fn verify_password(password: &str, phc: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(phc).map_err(|e| PasswordError::InvalidHash(e.to_string()))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(PasswordError::InvalidHash(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let phc = hash_password("correct horse", &PasswordParams::TEST).expect("hash");
        assert!(phc.starts_with("$argon2id$v=19$m=8,t=1,p=1$"), "{phc}");
        assert!(verify_password("correct horse", &phc).expect("verify"));
        assert!(!verify_password("battery staple", &phc).expect("verify"));
        let again = hash_password("correct horse", &PasswordParams::TEST).expect("hash");
        assert_ne!(phc, again, "salts must differ");
    }

    #[test]
    fn owasp_params_hash() {
        let phc = hash_password("pw", &PasswordParams::OWASP).expect("hash");
        assert!(phc.contains("m=19456,t=2,p=1"), "{phc}");
        assert!(verify_password("pw", &phc).expect("verify"));
    }

    #[test]
    fn bad_inputs() {
        assert!(verify_password("pw", "not a hash").is_err());
        let bad = PasswordParams {
            memory_kib: 1,
            ..PasswordParams::TEST
        };
        assert!(matches!(
            hash_password("pw", &bad),
            Err(PasswordError::Params(_))
        ));
    }
}
