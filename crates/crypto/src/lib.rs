//! sourcegull-crypto: envelope encryption for secrets at rest,
//! access-token and session-id utilities and password hashing.
//!
//! - [`KeyProvider`] supplies key-encryption keys (KEKs); [`FileKeyProvider`]
//!   reads them from a mounted file or Kubernetes Secret.
//! - [`Keyring`] holds one data key (DEK) per [`RecordClass`] and version,
//!   wrapped by a KEK, and encrypts/decrypts records with AES-256-GCM.
//! - [`access_token`], [`session`], [`password`] and [`pkce`] cover credentials.

pub mod access_token;
mod keyring;
mod keys;
pub mod password;
pub mod pkce;
pub mod session;

pub use keyring::{
    KeyState, Keyring, RecordClass, RotateScope, WrappedKey, ciphertext_dek_version,
};
pub use keys::{FileKeyProvider, KEY_LEN, Kek, KeyProvider, StaticKeyProvider};
pub use zeroize::Zeroizing;

/// Errors from key handling and encryption.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The key file could not be read.
    #[error("reading key file {path}: {source}")]
    KeyFile {
        /// Path of the key file.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A key (KEK) was malformed: bad base64, wrong length, duplicate id, etc.
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// The provider has no KEK with this id.
    #[error("unknown key-encryption key {0:?}")]
    UnknownKek(String),
    /// No active data key exists for the record class.
    #[error("no active data key for record class {0}")]
    NoActiveKey(RecordClass),
    /// The ciphertext refers to a data key version the keyring does not hold.
    #[error("unknown data key version {version} for record class {class}")]
    UnknownDek {
        /// Record class.
        class: RecordClass,
        /// Data key version from the ciphertext header.
        version: i32,
    },
    /// The blob is truncated or its header is malformed.
    #[error("malformed ciphertext: {0}")]
    Malformed(&'static str),
    /// The blob uses a format version this build does not understand.
    #[error("unsupported ciphertext format version {0}")]
    UnsupportedVersion(u8),
    /// Authentication failed: wrong key, wrong record class, or tampered data.
    #[error("decryption failed")]
    Decrypt,
    /// Encryption failed (only possible for absurdly large inputs).
    #[error("encryption failed")]
    Encrypt,
    /// A wrapped key or record class/state string from storage was invalid.
    #[error("invalid stored value: {0}")]
    InvalidStored(String),
}

/// Constant-time equality for secrets (MACs, token hashes). Unequal lengths compare false.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
