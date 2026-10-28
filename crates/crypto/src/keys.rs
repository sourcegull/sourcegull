//! Key-encryption keys (KEKs) and their providers.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use zeroize::Zeroizing;

use crate::CryptoError;

/// Length in bytes of every key (AES-256).
pub const KEY_LEN: usize = 32;

/// Maximum KEK id length; ids are stored in wrapped-key headers with a one-byte length.
const MAX_KEK_ID_LEN: usize = 255;

/// A key-encryption key: an id plus 32 secret bytes (zeroized on drop).
#[derive(Clone)]
pub struct Kek {
    id: String,
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl Kek {
    /// Creates a KEK. The id must be 1..=255 bytes and must not contain `:` or whitespace.
    pub fn new(id: impl Into<String>, key: [u8; KEY_LEN]) -> Result<Self, CryptoError> {
        let id = id.into();
        validate_id(&id)?;
        Ok(Self {
            id,
            key: Zeroizing::new(key),
        })
    }

    /// The KEK id, recorded in every data key it wraps.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.key
    }
}

impl fmt::Debug for Kek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kek")
            .field("id", &self.id)
            .field("key", &"<redacted>")
            .finish()
    }
}

fn validate_id(id: &str) -> Result<(), CryptoError> {
    if id.is_empty() || id.len() > MAX_KEK_ID_LEN {
        return Err(CryptoError::InvalidKey(format!(
            "key id must be 1..={MAX_KEK_ID_LEN} bytes"
        )));
    }
    if id.contains(':') || id.chars().any(char::is_whitespace) {
        return Err(CryptoError::InvalidKey(format!(
            "key id {id:?} must not contain ':' or whitespace"
        )));
    }
    Ok(())
}

/// Source of key-encryption keys. Vault or a KMS could implement it too.
pub trait KeyProvider: Send + Sync {
    /// Returns the KEK with this id (old ids stay available during rotation).
    fn kek(&self, id: &str) -> Result<Kek, CryptoError>;
    /// The id of the KEK used to wrap new data keys.
    fn current_kek_id(&self) -> &str;
}

/// In-memory set of KEKs; the last one added is current.
#[derive(Debug, Clone)]
pub struct StaticKeyProvider {
    keys: HashMap<String, Kek>,
    current: String,
}

impl StaticKeyProvider {
    /// Creates a provider holding a single KEK.
    pub fn new(id: impl Into<String>, key: [u8; KEY_LEN]) -> Result<Self, CryptoError> {
        let kek = Kek::new(id, key)?;
        let current = kek.id.clone();
        Ok(Self {
            keys: HashMap::from([(current.clone(), kek)]),
            current,
        })
    }

    /// Adds a KEK and makes it current. Duplicate ids are rejected.
    pub fn with_key(
        mut self,
        id: impl Into<String>,
        key: [u8; KEY_LEN],
    ) -> Result<Self, CryptoError> {
        let kek = Kek::new(id, key)?;
        if self.keys.contains_key(&kek.id) {
            return Err(CryptoError::InvalidKey(format!(
                "duplicate key id {:?}",
                kek.id
            )));
        }
        self.current = kek.id.clone();
        self.keys.insert(kek.id.clone(), kek);
        Ok(self)
    }
}

impl KeyProvider for StaticKeyProvider {
    fn kek(&self, id: &str) -> Result<Kek, CryptoError> {
        self.keys
            .get(id)
            .cloned()
            .ok_or_else(|| CryptoError::UnknownKek(id.to_owned()))
    }

    fn current_kek_id(&self) -> &str {
        &self.current
    }
}

/// KEKs loaded from a file (typically a mounted Kubernetes Secret).
///
/// Format: one key per line, either `id:base64` or a bare `base64` (id `default`).
/// Each key is 32 bytes, standard base64. Blank lines and `#` comments are
/// ignored. The **last** key is current, so rotation means appending a line;
/// keep old lines until data keys have been re-wrapped.
#[derive(Debug, Clone)]
pub struct FileKeyProvider {
    inner: StaticKeyProvider,
}

impl FileKeyProvider {
    /// Reads and parses the key file at `path`.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, CryptoError> {
        let path = path.as_ref();
        let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|source| {
            CryptoError::KeyFile {
                path: path.display().to_string(),
                source,
            }
        })?);
        Self::parse(&text)
    }

    /// Parses key-file contents (see the type docs for the format).
    pub fn parse(text: &str) -> Result<Self, CryptoError> {
        Self::parse_entries(text.lines(), "line")
    }

    /// Parses keys given inline (`SOURCEGULL_KEYS`): the key-file format, with entries separated
    /// by newlines or commas, e.g. `k1:<base64>,k2:<base64>`. The last entry is current.
    pub fn parse_inline(text: &str) -> Result<Self, CryptoError> {
        Self::parse_entries(text.split(['\n', ',']), "entry")
    }

    fn parse_entries<'a>(
        entries: impl Iterator<Item = &'a str>,
        unit: &str,
    ) -> Result<Self, CryptoError> {
        let mut provider: Option<StaticKeyProvider> = None;
        for (lineno, line) in entries.enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (id, b64) = line.split_once(':').unwrap_or(("default", line));
            let key = decode_key(b64.trim())
                .map_err(|e| CryptoError::InvalidKey(format!("{unit} {}: {e}", lineno + 1)))?;
            provider = Some(match provider {
                None => StaticKeyProvider::new(id.trim(), key)?,
                Some(p) => p.with_key(id.trim(), key)?,
            });
        }
        provider
            .map(|inner| Self { inner })
            .ok_or_else(|| CryptoError::InvalidKey("no keys given".to_owned()))
    }
}

fn decode_key(b64: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = Zeroizing::new(
        STANDARD
            .decode(b64)
            .map_err(|_| "invalid base64".to_owned())?,
    );
    <[u8; KEY_LEN]>::try_from(bytes.as_slice())
        .map_err(|_| format!("key must be {KEY_LEN} bytes, got {}", bytes.len()))
}

impl KeyProvider for FileKeyProvider {
    fn kek(&self, id: &str) -> Result<Kek, CryptoError> {
        self.inner.kek(id)
    }

    fn current_kek_id(&self) -> &str {
        self.inner.current_kek_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(byte: u8) -> String {
        STANDARD.encode([byte; KEY_LEN])
    }

    #[test]
    fn parses_bare_key() {
        let p = FileKeyProvider::parse(&format!("{}\n", b64(1))).expect("parse");
        assert_eq!(p.current_kek_id(), "default");
        assert_eq!(p.kek("default").expect("kek").bytes(), &[1; KEY_LEN]);
    }

    #[test]
    fn parses_rotation_file() {
        let text = format!("# keys\nk1:{}\n\nk2: {}\n", b64(1), b64(2));
        let p = FileKeyProvider::parse(&text).expect("parse");
        assert_eq!(p.current_kek_id(), "k2");
        assert_eq!(p.kek("k1").expect("k1").bytes(), &[1; KEY_LEN]);
        assert!(matches!(p.kek("k3"), Err(CryptoError::UnknownKek(_))));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(FileKeyProvider::parse("").is_err());
        assert!(FileKeyProvider::parse("k1:not-base64!").is_err());
        assert!(FileKeyProvider::parse(&format!("k1:{}", STANDARD.encode([0u8; 16]))).is_err());
        assert!(FileKeyProvider::parse(&format!("k1:{}\nk1:{}", b64(1), b64(2))).is_err());
    }

    #[test]
    fn parses_inline_keys() {
        let p =
            FileKeyProvider::parse_inline(&format!("k1:{}, k2:{}", b64(1), b64(2))).expect("parse");
        assert_eq!(p.current_kek_id(), "k2");
        assert_eq!(p.kek("k1").expect("k1").bytes(), &[1; KEY_LEN]);
        let p = FileKeyProvider::parse_inline(&format!("k1:{}\nk2:{}\n", b64(1), b64(2)))
            .expect("parse newlines");
        assert_eq!(p.current_kek_id(), "k2");
        let err = FileKeyProvider::parse_inline(&format!("k1:{},k2:nope", b64(1)))
            .expect_err("bad entry");
        assert!(err.to_string().contains("entry 2"), "{err}");
        assert!(FileKeyProvider::parse_inline(" , ").is_err());
    }

    #[test]
    fn reads_from_path() {
        let dir = std::env::temp_dir().join(format!("sourcegull-crypto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("kek");
        std::fs::write(&path, format!("main:{}\n", b64(9))).expect("write");
        let p = FileKeyProvider::from_path(&path).expect("load");
        assert_eq!(p.current_kek_id(), "main");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert!(matches!(
            FileKeyProvider::from_path(&path),
            Err(CryptoError::KeyFile { .. })
        ));
    }

    #[test]
    fn kek_debug_is_redacted() {
        let k = Kek::new("x", [7; KEY_LEN]).expect("kek");
        assert!(!format!("{k:?}").contains('7'));
    }
}
