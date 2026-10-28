//! Envelope encryption with per-record-class data keys.
//!
//! Each [`RecordClass`] has one or more data keys (DEKs), identified by a
//! version. DEKs are random 32-byte AES-256-GCM keys, stored only in wrapped
//! form ([`WrappedKey`], persisted by callers in `encryption_keys`), wrapped by a
//! KEK from a [`KeyProvider`].
//!
//! Wrapped DEK layout (v1):
//! `0x01 | kek_id_len: u8 | kek_id | nonce: [u8; 12] | AES-GCM(dek) + tag`,
//! with AAD = `"sourcegull-dek\x01" | class | version (u32 BE)`.
//!
//! Record ciphertext layout (v1):
//! `0x01 | dek_version: u32 BE | nonce: [u8; 12] | AES-GCM(plaintext) + tag`,
//! with AAD = `"sourcegull-data\x01" | class | dek_version (u32 BE)`, so a blob
//! cannot be decrypted as another record class or under another key version.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use rand::rngs::OsRng;
use zeroize::Zeroizing;

use crate::CryptoError;
use crate::keys::{KEY_LEN, Kek, KeyProvider};

const FORMAT_V1: u8 = 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const DEK_AAD_PREFIX: &[u8] = b"sourcegull-dek\x01";
const DATA_AAD_PREFIX: &[u8] = b"sourcegull-data\x01";
/// Header length of a v1 record ciphertext: format byte + version + nonce.
const DATA_HEADER_LEN: usize = 1 + 4 + NONCE_LEN;

/// Kinds of secret records, each with its own data keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordClass {
    /// Code-host connection configs (`external_services.config`).
    ExternalServiceConfig,
    /// Per-user code-host credentials.
    UserCredential,
    /// OAuth access/refresh tokens.
    OAuthToken,
    /// Executor secrets.
    ExecutorSecret,
    /// Incoming webhook secrets.
    WebhookSecret,
    /// Site-wide credentials.
    SiteCredential,
    /// Outbound webhooks: their URLs and secrets, queued event payloads and delivery logs.
    OutboundWebhook,
    /// Watch webhook URLs, Telegram bot tokens and email headers (`cm_actions.url_enc`,
    /// `cm_actions.header_enc`).
    CodeMonitorWebhook,
}

impl RecordClass {
    /// Every record class.
    pub const ALL: [RecordClass; 8] = [
        Self::ExternalServiceConfig,
        Self::UserCredential,
        Self::OAuthToken,
        Self::ExecutorSecret,
        Self::WebhookSecret,
        Self::SiteCredential,
        Self::OutboundWebhook,
        Self::CodeMonitorWebhook,
    ];

    /// Stable storage name (the `encryption_keys.id` value).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExternalServiceConfig => "external_service_config",
            Self::UserCredential => "user_credential",
            Self::OAuthToken => "oauth_token",
            Self::ExecutorSecret => "executor_secret",
            Self::WebhookSecret => "webhook_secret",
            Self::SiteCredential => "site_credential",
            Self::OutboundWebhook => "outbound_webhook",
            Self::CodeMonitorWebhook => "code_monitor_webhook",
        }
    }
}

impl fmt::Display for RecordClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RecordClass {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| CryptoError::InvalidStored(format!("unknown record class {s:?}")))
    }
}

/// Lifecycle state of a data key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyState {
    /// Usable for encryption and decryption.
    Active,
    /// Decryption only (kept until records are re-encrypted).
    Retired,
}

impl KeyState {
    /// Stable storage name (the `encryption_keys.state` value).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

impl FromStr for KeyState {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            _ => Err(CryptoError::InvalidStored(format!(
                "unknown key state {s:?}"
            ))),
        }
    }
}

/// A data key in its persisted, KEK-wrapped form (one `encryption_keys` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedKey {
    /// Record class (`encryption_keys.id`).
    pub class: RecordClass,
    /// Version, starting at 1 (`encryption_keys.version`).
    pub version: i32,
    /// Wrapped key bytes, including the KEK id (`encryption_keys.wrapped_key`).
    pub wrapped: Vec<u8>,
    /// Lifecycle state (`encryption_keys.state`).
    pub state: KeyState,
}

impl WrappedKey {
    /// The id of the KEK that wrapped this key.
    pub fn kek_id(&self) -> Result<&str, CryptoError> {
        parse_wrapped(&self.wrapped).map(|(id, _, _)| id)
    }
}

struct Dek {
    key: Zeroizing<[u8; KEY_LEN]>,
    wrapped: WrappedKey,
}

type Deks = BTreeMap<(RecordClass, i32), Dek>;

/// Which record classes [`Keyring::rotate`] gives a new data key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateScope {
    /// Only classes whose active data key is wrapped by a KEK other than the provider's
    /// current one (the key file gained a new last line). Nothing happens otherwise.
    KekChanged,
    /// Every class (a new data key even though the KEK did not change).
    All,
}

/// Unwrapped data keys for all record classes, plus the KEK provider.
///
/// Encryption uses the highest-versioned [`KeyState::Active`] key of the class (the
/// primary key); decryption uses whichever version the ciphertext names, so retired keys
/// stay decrypt-only until the re-encryption migration has moved every record off them.
///
/// The keys sit behind a lock so a shared keyring (`Arc<Keyring>`) can pick up keys another
/// process created after a rotation ([`Keyring::reload`]); `&mut self` methods change the
/// keyring before it is shared.
pub struct Keyring {
    provider: RwLock<Arc<dyn KeyProvider>>,
    deks: RwLock<Deks>,
}

impl fmt::Debug for Keyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyring")
            .field("current_kek_id", &self.current_kek_id())
            .field(
                "deks",
                &read(&self.deks).keys().copied().collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    // A panic while holding the lock cannot leave the map half-updated (writers swap whole
    // values), so a poisoned lock is still consistent.
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn get_mut<T>(lock: &mut RwLock<T>) -> &mut T {
    lock.get_mut().unwrap_or_else(PoisonError::into_inner)
}

impl Keyring {
    /// Creates a keyring with no data keys; add them with [`Keyring::generate_dek`]
    /// or [`Keyring::ensure_deks`].
    pub fn new(provider: Arc<dyn KeyProvider>) -> Self {
        Self {
            provider: RwLock::new(provider),
            deks: RwLock::new(BTreeMap::new()),
        }
    }

    /// Loads persisted data keys, unwrapping each with the KEK it names.
    pub fn from_wrapped(
        provider: Arc<dyn KeyProvider>,
        keys: impl IntoIterator<Item = WrappedKey>,
    ) -> Result<Self, CryptoError> {
        let deks = unwrap_all(provider.as_ref(), keys)?;
        Ok(Self {
            provider: RwLock::new(provider),
            deks: RwLock::new(deks),
        })
    }

    /// Replaces the data keys (and, if given, the KEK provider) of a shared keyring, e.g.
    /// after another process rotated keys. On error the keyring is unchanged.
    pub fn reload(
        &self,
        provider: Option<Arc<dyn KeyProvider>>,
        keys: impl IntoIterator<Item = WrappedKey>,
    ) -> Result<(), CryptoError> {
        let provider = provider.unwrap_or_else(|| read(&self.provider).clone());
        let deks = unwrap_all(provider.as_ref(), keys)?;
        *self
            .provider
            .write()
            .unwrap_or_else(PoisonError::into_inner) = provider;
        *self.deks.write().unwrap_or_else(PoisonError::into_inner) = deks;
        Ok(())
    }

    /// The id of the KEK that wraps new data keys (the last line of the key file).
    pub fn current_kek_id(&self) -> String {
        read(&self.provider).current_kek_id().to_owned()
    }

    /// Generates a new active data key for `class` (version = highest + 1),
    /// wrapped by the provider's current KEK. The caller must persist the result.
    pub fn generate_dek(&mut self, class: RecordClass) -> Result<WrappedKey, CryptoError> {
        let provider = get_mut(&mut self.provider).clone();
        let deks = get_mut(&mut self.deks);
        let version = deks
            .range((class, i32::MIN)..=(class, i32::MAX))
            .next_back()
            .map_or(Ok(1), |(&(_, v), _)| {
                v.checked_add(1).ok_or(CryptoError::InvalidStored(
                    "data key version overflow".into(),
                ))
            })?;
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        OsRng.fill_bytes(key.as_mut());
        let kek = provider.kek(provider.current_kek_id())?;
        let wrapped = WrappedKey {
            class,
            version,
            wrapped: wrap_dek(&kek, class, version, &key)?,
            state: KeyState::Active,
        };
        deks.insert(
            (class, version),
            Dek {
                key,
                wrapped: wrapped.clone(),
            },
        );
        Ok(wrapped)
    }

    /// Generates a data key for every class without an active one; returns the
    /// new keys for the caller to persist (empty when nothing was missing).
    pub fn ensure_deks(&mut self) -> Result<Vec<WrappedKey>, CryptoError> {
        let mut created = Vec::new();
        for class in RecordClass::ALL {
            if self.current_version(class).is_none() {
                created.push(self.generate_dek(class)?);
            }
        }
        Ok(created)
    }

    /// Marks a data key decrypt-only; returns the updated row for the caller to persist.
    pub fn retire(&mut self, class: RecordClass, version: i32) -> Result<WrappedKey, CryptoError> {
        let dek = get_mut(&mut self.deks)
            .get_mut(&(class, version))
            .ok_or(CryptoError::UnknownDek { class, version })?;
        dek.wrapped.state = KeyState::Retired;
        Ok(dek.wrapped.clone())
    }

    /// Re-wraps every data key with the KEK `new_kek_id` and returns the rows to
    /// persist. Data keys (and so existing ciphertexts) are unchanged.
    pub fn rewrap(&mut self, new_kek_id: &str) -> Result<Vec<WrappedKey>, CryptoError> {
        let kek = get_mut(&mut self.provider).kek(new_kek_id)?;
        let deks = get_mut(&mut self.deks);
        // Wrap everything first so a failure leaves the keyring unchanged.
        let rewrapped = deks
            .iter()
            .map(|(&(class, version), dek)| {
                wrap_dek(&kek, class, version, &dek.key).map(|w| ((class, version), w))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rewrapped
            .into_iter()
            .filter_map(|(k, wrapped)| {
                deks.get_mut(&k).map(|dek| {
                    dek.wrapped.wrapped = wrapped;
                    dek.wrapped.clone()
                })
            })
            .collect())
    }

    /// Whether some class's active data key is wrapped by a KEK other than the current one,
    /// i.e. [`RotateScope::KekChanged`] would rotate.
    pub fn needs_rotation(&self) -> bool {
        let current = self.current_kek_id();
        let deks = read(&self.deks);
        RecordClass::ALL.into_iter().any(|class| {
            active(&deks, class)
                .is_some_and(|(_, d)| d.wrapped.kek_id().ok() != Some(current.as_str()))
        })
    }

    /// Rotates keys: re-wraps every data key with the current KEK (so older KEKs are
    /// no longer needed to unwrap them) and, for each class in `scope`, retires the active
    /// data key and generates a new one. Records keep their old data key version until the
    /// re-encryption out-of-band migration rewrites them; retired keys still decrypt.
    ///
    /// Returns the rows to persist, ordered by class and version (a class's retired key
    /// before its new active one), or nothing when there was nothing to rotate.
    pub fn rotate(&mut self, scope: RotateScope) -> Result<Vec<WrappedKey>, CryptoError> {
        let current = self.current_kek_id();
        let classes: Vec<RecordClass> = {
            let deks = get_mut(&mut self.deks);
            RecordClass::ALL
                .into_iter()
                .filter(|&class| match scope {
                    RotateScope::All => true,
                    RotateScope::KekChanged => active(deks, class)
                        .is_some_and(|(_, d)| d.wrapped.kek_id().ok() != Some(current.as_str())),
                })
                .collect()
        };
        let stale_wrapping = get_mut(&mut self.deks)
            .values()
            .any(|d| d.wrapped.kek_id().ok() != Some(current.as_str()));
        if classes.is_empty() && !stale_wrapping {
            return Ok(Vec::new());
        }
        let mut changed: BTreeMap<(RecordClass, i32), WrappedKey> = BTreeMap::new();
        if stale_wrapping {
            for k in self.rewrap(&current)? {
                changed.insert((k.class, k.version), k);
            }
        }
        for class in classes {
            if let Some(version) = self.current_version(class) {
                let k = self.retire(class, version)?;
                changed.insert((class, version), k);
            }
            let k = self.generate_dek(class)?;
            changed.insert((class, k.version), k);
        }
        Ok(changed.into_values().collect())
    }

    /// All data keys in wrapped form, ordered by class and version.
    pub fn wrapped_keys(&self) -> Vec<WrappedKey> {
        read(&self.deks)
            .values()
            .map(|d| d.wrapped.clone())
            .collect()
    }

    /// The version used for new encryptions of `class` (its primary key), if any.
    pub fn current_version(&self, class: RecordClass) -> Option<i32> {
        active(&read(&self.deks), class).map(|(v, _)| v)
    }

    /// Encrypts `plaintext` for `class` with its current data key.
    pub fn encrypt(&self, class: RecordClass, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let deks = read(&self.deks);
        let (version, dek) = active(&deks, class).ok_or(CryptoError::NoActiveKey(class))?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher(&dek.key)
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad(DATA_AAD_PREFIX, class, version),
                },
            )
            .map_err(|_| CryptoError::Encrypt)?;
        let mut out = Vec::with_capacity(DATA_HEADER_LEN + ct.len());
        out.push(FORMAT_V1);
        out.extend_from_slice(&version_bytes(version));
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypts a blob produced by [`Keyring::encrypt`] for the same `class`.
    pub fn decrypt(
        &self,
        class: RecordClass,
        blob: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let version = ciphertext_dek_version(blob)?;
        if blob.len() < DATA_HEADER_LEN + TAG_LEN {
            return Err(CryptoError::Malformed("ciphertext too short"));
        }
        let deks = read(&self.deks);
        let dek = deks
            .get(&(class, version))
            .ok_or(CryptoError::UnknownDek { class, version })?;
        let nonce = &blob[5..DATA_HEADER_LEN];
        cipher(&dek.key)
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: &blob[DATA_HEADER_LEN..],
                    aad: &aad(DATA_AAD_PREFIX, class, version),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| CryptoError::Decrypt)
    }
}

/// The highest-versioned active data key of `class`.
fn active(deks: &Deks, class: RecordClass) -> Option<(i32, &Dek)> {
    deks.range((class, i32::MIN)..=(class, i32::MAX))
        .rev()
        .find(|(_, d)| d.wrapped.state == KeyState::Active)
        .map(|(&(_, v), d)| (v, d))
}

fn unwrap_all(
    provider: &dyn KeyProvider,
    keys: impl IntoIterator<Item = WrappedKey>,
) -> Result<Deks, CryptoError> {
    let mut deks = BTreeMap::new();
    for wk in keys {
        if wk.version < 1 {
            return Err(CryptoError::InvalidStored(format!(
                "data key version {} for {} must be positive",
                wk.version, wk.class
            )));
        }
        let (kek_id, nonce, ct) = parse_wrapped(&wk.wrapped)?;
        let kek = provider.kek(kek_id)?;
        let key = unwrap_dek(&kek, wk.class, wk.version, nonce, ct)?;
        if deks.contains_key(&(wk.class, wk.version)) {
            return Err(CryptoError::InvalidStored(format!(
                "duplicate data key {} v{}",
                wk.class, wk.version
            )));
        }
        deks.insert((wk.class, wk.version), Dek { key, wrapped: wk });
    }
    Ok(deks)
}

/// Reads the data key version from a record ciphertext header, e.g. to find
/// records that the re-encryption migration still has to rewrite.
pub fn ciphertext_dek_version(blob: &[u8]) -> Result<i32, CryptoError> {
    match blob.first() {
        None => Err(CryptoError::Malformed("empty ciphertext")),
        Some(&FORMAT_V1) => {
            let raw: [u8; 4] = blob
                .get(1..5)
                .and_then(|b| b.try_into().ok())
                .ok_or(CryptoError::Malformed("ciphertext too short"))?;
            i32::try_from(u32::from_be_bytes(raw))
                .ok()
                .filter(|v| *v >= 1)
                .ok_or(CryptoError::Malformed("invalid data key version"))
        }
        Some(&v) => Err(CryptoError::UnsupportedVersion(v)),
    }
}

fn cipher(key: &[u8; KEY_LEN]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))
}

fn version_bytes(version: i32) -> [u8; 4] {
    // Versions are validated positive before reaching here.
    (version as u32).to_be_bytes()
}

fn aad(prefix: &[u8], class: RecordClass, version: i32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(prefix.len() + 32);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(class.as_str().as_bytes());
    aad.extend_from_slice(&version_bytes(version));
    aad
}

fn wrap_dek(
    kek: &Kek,
    class: RecordClass,
    version: i32,
    dek: &[u8; KEY_LEN],
) -> Result<Vec<u8>, CryptoError> {
    let id = kek.id().as_bytes();
    let id_len =
        u8::try_from(id.len()).map_err(|_| CryptoError::InvalidKey("key id too long".into()))?;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher(kek.bytes())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: dek,
                aad: &aad(DEK_AAD_PREFIX, class, version),
            },
        )
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(2 + id.len() + NONCE_LEN + ct.len());
    out.push(FORMAT_V1);
    out.push(id_len);
    out.extend_from_slice(id);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Splits a wrapped key into (kek id, nonce, ciphertext+tag).
fn parse_wrapped(wrapped: &[u8]) -> Result<(&str, &[u8], &[u8]), CryptoError> {
    let (&format, rest) = wrapped
        .split_first()
        .ok_or(CryptoError::Malformed("empty wrapped key"))?;
    if format != FORMAT_V1 {
        return Err(CryptoError::UnsupportedVersion(format));
    }
    let (&id_len, rest) = rest
        .split_first()
        .ok_or(CryptoError::Malformed("wrapped key too short"))?;
    let id_len = usize::from(id_len);
    if rest.len() != id_len + NONCE_LEN + KEY_LEN + TAG_LEN {
        return Err(CryptoError::Malformed("wrapped key has wrong length"));
    }
    let (id, rest) = rest.split_at(id_len);
    let id = std::str::from_utf8(id).map_err(|_| CryptoError::Malformed("wrapped key id"))?;
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    Ok((id, nonce, ct))
}

fn unwrap_dek(
    kek: &Kek,
    class: RecordClass,
    version: i32,
    nonce: &[u8],
    ct: &[u8],
) -> Result<Zeroizing<[u8; KEY_LEN]>, CryptoError> {
    let plain = Zeroizing::new(
        cipher(kek.bytes())
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: &aad(DEK_AAD_PREFIX, class, version),
                },
            )
            .map_err(|_| CryptoError::Decrypt)?,
    );
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    if plain.len() != KEY_LEN {
        return Err(CryptoError::Malformed("unwrapped key has wrong length"));
    }
    key.copy_from_slice(&plain);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StaticKeyProvider;

    fn provider() -> Arc<StaticKeyProvider> {
        Arc::new(StaticKeyProvider::new("k1", [1; KEY_LEN]).expect("provider"))
    }

    fn ring() -> Keyring {
        let mut ring = Keyring::new(provider());
        let created = ring.ensure_deks().expect("ensure");
        assert_eq!(created.len(), RecordClass::ALL.len());
        ring
    }

    #[test]
    fn roundtrip_all_classes() {
        let ring = ring();
        for class in RecordClass::ALL {
            let blob = ring.encrypt(class, b"s3cret").expect("encrypt");
            assert_eq!(blob[0], FORMAT_V1);
            assert_eq!(ciphertext_dek_version(&blob).expect("version"), 1);
            assert_eq!(
                ring.decrypt(class, &blob).expect("decrypt").as_slice(),
                b"s3cret"
            );
        }
        let empty = ring.encrypt(RecordClass::OAuthToken, b"").expect("encrypt");
        assert!(
            ring.decrypt(RecordClass::OAuthToken, &empty)
                .expect("decrypt")
                .is_empty()
        );
    }

    #[test]
    fn nonces_differ() {
        let ring = ring();
        let a = ring.encrypt(RecordClass::WebhookSecret, b"x").expect("a");
        let b = ring.encrypt(RecordClass::WebhookSecret, b"x").expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_class_fails() {
        let ring = ring();
        let blob = ring
            .encrypt(RecordClass::UserCredential, b"pw")
            .expect("encrypt");
        assert!(matches!(
            ring.decrypt(RecordClass::SiteCredential, &blob),
            Err(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn wrong_class_fails_even_with_shared_key_material() {
        // AAD alone must separate classes: copy one class's DEK under another class.
        let mut ring = ring();
        let blob = ring
            .encrypt(RecordClass::UserCredential, b"pw")
            .expect("encrypt");
        let deks = get_mut(&mut ring.deks);
        let key = deks[&(RecordClass::UserCredential, 1)].key.clone();
        if let Some(dek) = deks.get_mut(&(RecordClass::SiteCredential, 1)) {
            dek.key = key;
        }
        assert!(ring.decrypt(RecordClass::SiteCredential, &blob).is_err());
    }

    #[test]
    fn tampering_fails() {
        let ring = ring();
        let blob = ring
            .encrypt(RecordClass::ExecutorSecret, b"payload")
            .expect("encrypt");
        for i in 0..blob.len() {
            let mut bad = blob.clone();
            bad[i] ^= 0x01;
            assert!(
                ring.decrypt(RecordClass::ExecutorSecret, &bad).is_err(),
                "byte {i}"
            );
        }
        assert!(
            ring.decrypt(RecordClass::ExecutorSecret, &blob[..blob.len() - 1])
                .is_err()
        );
        assert!(ring.decrypt(RecordClass::ExecutorSecret, &[]).is_err());
        assert!(matches!(
            ring.decrypt(RecordClass::ExecutorSecret, &[9, 0, 0, 0, 1]),
            Err(CryptoError::UnsupportedVersion(9))
        ));
    }

    #[test]
    fn no_active_key() {
        let ring = Keyring::new(provider());
        assert!(matches!(
            ring.encrypt(RecordClass::OAuthToken, b"x"),
            Err(CryptoError::NoActiveKey(RecordClass::OAuthToken))
        ));
    }

    #[test]
    fn persisted_keys_reload() {
        let p = provider();
        let ring = ring();
        let blob = ring
            .encrypt(RecordClass::OAuthToken, b"tok")
            .expect("encrypt");
        let reloaded = Keyring::from_wrapped(p, ring.wrapped_keys()).expect("reload");
        assert_eq!(
            reloaded
                .decrypt(RecordClass::OAuthToken, &blob)
                .expect("decrypt")
                .as_slice(),
            b"tok"
        );
    }

    #[test]
    fn wrapped_key_is_bound_to_class_and_version() {
        let ring = ring();
        let mut keys = ring.wrapped_keys();
        keys[0].version = 2;
        assert!(matches!(
            Keyring::from_wrapped(provider(), keys.clone()),
            Err(CryptoError::Decrypt)
        ));
        keys[0].version = 1;
        keys[0].class = keys[1].class;
        assert!(Keyring::from_wrapped(provider(), keys).is_err());
    }

    #[test]
    fn dek_rotation_keeps_old_ciphertexts_readable() {
        let mut ring = ring();
        let class = RecordClass::ExternalServiceConfig;
        let old = ring.encrypt(class, b"v1 data").expect("old");
        let new_key = ring.generate_dek(class).expect("generate");
        assert_eq!(new_key.version, 2);
        let new = ring.encrypt(class, b"v2 data").expect("new");
        assert_eq!(ciphertext_dek_version(&new).expect("version"), 2);
        assert_eq!(
            ring.decrypt(class, &old).expect("old").as_slice(),
            b"v1 data"
        );
        assert_eq!(
            ring.decrypt(class, &new).expect("new").as_slice(),
            b"v2 data"
        );

        // Retiring v2 makes v1 current again for encryption; both still decrypt.
        let retired = ring.retire(class, 2).expect("retire");
        assert_eq!(retired.state, KeyState::Retired);
        assert_eq!(ring.current_version(class), Some(1));
        assert!(ring.decrypt(class, &new).is_ok());
    }

    #[test]
    fn kek_rotation_rewraps_keys() {
        let rotated = Arc::new(
            StaticKeyProvider::new("k1", [1; KEY_LEN])
                .and_then(|p| p.with_key("k2", [2; KEY_LEN]))
                .expect("provider"),
        );
        let mut ring = Keyring::new(provider());
        ring.ensure_deks().expect("ensure");
        let blob = ring
            .encrypt(RecordClass::UserCredential, b"pw")
            .expect("encrypt");

        let mut ring = Keyring::from_wrapped(rotated.clone(), ring.wrapped_keys()).expect("load");
        let rewrapped = ring.rewrap(rotated.current_kek_id()).expect("rewrap");
        assert_eq!(rewrapped.len(), RecordClass::ALL.len());
        assert!(rewrapped.iter().all(|k| k.kek_id().ok() == Some("k2")));

        // After rotation, only the new KEK is needed.
        let only_new = Arc::new(StaticKeyProvider::new("k2", [2; KEY_LEN]).expect("provider"));
        let ring = Keyring::from_wrapped(only_new, rewrapped).expect("load rewrapped");
        assert_eq!(
            ring.decrypt(RecordClass::UserCredential, &blob)
                .expect("decrypt")
                .as_slice(),
            b"pw"
        );
        // New DEKs are wrapped with the current KEK.
        assert!(matches!(
            Keyring::from_wrapped(provider(), ring.wrapped_keys()),
            Err(CryptoError::UnknownKek(_))
        ));
    }

    fn two_keks() -> Arc<StaticKeyProvider> {
        Arc::new(
            StaticKeyProvider::new("k1", [1; KEY_LEN])
                .and_then(|p| p.with_key("k2", [2; KEY_LEN]))
                .expect("provider"),
        )
    }

    #[test]
    fn rotation_after_a_new_kek_retires_old_data_keys() {
        let mut ring = ring();
        assert!(!ring.needs_rotation());
        assert!(
            ring.rotate(RotateScope::KekChanged)
                .expect("noop")
                .is_empty()
        );
        let class = RecordClass::OAuthToken;
        let old = ring.encrypt(class, b"token").expect("encrypt");

        // The key file gains k2 (now current): every class rotates.
        let mut ring = Keyring::from_wrapped(two_keks(), ring.wrapped_keys()).expect("load");
        assert!(ring.needs_rotation());
        let changed = ring.rotate(RotateScope::KekChanged).expect("rotate");
        // Per class: v1 rewrapped + retired, v2 new.
        assert_eq!(changed.len(), 2 * RecordClass::ALL.len());
        assert!(changed.iter().all(|k| k.kek_id().ok() == Some("k2")));
        assert!(
            changed
                .iter()
                .all(|k| (k.version == 1) == (k.state == KeyState::Retired))
        );
        assert!(!ring.needs_rotation());
        assert_eq!(ring.current_version(class), Some(2));
        let new = ring.encrypt(class, b"token").expect("encrypt");
        assert_eq!(ciphertext_dek_version(&new).expect("version"), 2);

        // Only k2 is needed from now on; old ciphertexts stay readable (decrypt-only key).
        let only_k2 = Arc::new(StaticKeyProvider::new("k2", [2; KEY_LEN]).expect("provider"));
        let ring = Keyring::from_wrapped(only_k2, ring.wrapped_keys()).expect("reload");
        assert_eq!(ring.decrypt(class, &old).expect("old").as_slice(), b"token");
        assert_eq!(ring.decrypt(class, &new).expect("new").as_slice(), b"token");
    }

    #[test]
    fn forced_rotation_keeps_the_kek() {
        let mut ring = ring();
        let changed = ring.rotate(RotateScope::All).expect("rotate");
        assert_eq!(changed.len(), 2 * RecordClass::ALL.len());
        assert!(changed.iter().all(|k| k.kek_id().ok() == Some("k1")));
        for class in RecordClass::ALL {
            assert_eq!(ring.current_version(class), Some(2));
        }
    }

    #[test]
    fn shared_keyring_reloads_keys_rotated_elsewhere() {
        let p: Arc<dyn KeyProvider> = provider();
        let mut writer = ring();
        let shared =
            Arc::new(Keyring::from_wrapped(p.clone(), writer.wrapped_keys()).expect("load"));
        writer.rotate(RotateScope::All).expect("rotate");
        let blob = writer
            .encrypt(RecordClass::ExecutorSecret, b"v2")
            .expect("encrypt");
        assert!(matches!(
            shared.decrypt(RecordClass::ExecutorSecret, &blob),
            Err(CryptoError::UnknownDek { version: 2, .. })
        ));
        shared.reload(None, writer.wrapped_keys()).expect("reload");
        assert_eq!(
            shared
                .decrypt(RecordClass::ExecutorSecret, &blob)
                .expect("decrypt")
                .as_slice(),
            b"v2"
        );
        assert_eq!(shared.current_version(RecordClass::ExecutorSecret), Some(2));
        // A failed reload (unknown KEK) leaves the keyring as it was.
        let other = Arc::new(StaticKeyProvider::new("zz", [3; KEY_LEN]).expect("provider"));
        assert!(shared.reload(Some(other), writer.wrapped_keys()).is_err());
        assert_eq!(shared.current_kek_id(), "k1");
        assert!(shared.decrypt(RecordClass::ExecutorSecret, &blob).is_ok());
    }

    #[test]
    fn storage_names_roundtrip() {
        for class in RecordClass::ALL {
            assert_eq!(class.as_str().parse::<RecordClass>().ok(), Some(class));
        }
        assert_eq!("retired".parse::<KeyState>().ok(), Some(KeyState::Retired));
        assert!("bogus".parse::<KeyState>().is_err());
    }
}
