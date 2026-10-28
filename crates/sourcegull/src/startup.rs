//! Startup steps shared by `run`, `migrate` and `supervise`: database connection with retry,
//! the schema check and encryption keys.

use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rand::RngCore as _;
use sourcegull_crypto::{FileKeyProvider, KEY_LEN, KeyProvider as _, Zeroizing};
use sourcegull_db::migrate::{self, MigrationProblem};
use sourcegull_db::sqlx::PgPool;
use sourcegull_db::{Db, DbOptions};
use tokio_util::sync::CancellationToken;

/// How long startup waits for the database to accept connections.
pub const DB_CONNECT_DEADLINE: Duration = Duration::from_secs(60);

/// Connects to `url`, retrying with exponential backoff (0.5 s up to 5 s between attempts)
/// until `deadline` has passed or `cancel` fires.
pub async fn connect_with_retry(
    url: &str,
    opts: &DbOptions,
    deadline: Duration,
    cancel: &CancellationToken,
) -> anyhow::Result<Db> {
    let start = Instant::now();
    let mut delay = Duration::from_millis(500);
    let mut attempt = 1u32;
    loop {
        match Db::connect(url, opts).await {
            Ok(db) => return Ok(db),
            Err(e) if start.elapsed() + delay < deadline => {
                tracing::warn!(attempt, error = %e, "database not available yet; retrying in {delay:?}");
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("connecting to the database (gave up after {attempt} attempts)")
                });
            }
        }
        tokio::select! {
            () = cancel.cancelled() => bail!("cancelled while waiting for the database"),
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(5));
        attempt += 1;
    }
}

/// Whether the database schema matches this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// Every embedded migration is applied and the history is consistent.
    Current,
    /// Some embedded migrations are not applied yet.
    Pending {
        /// Versions not applied yet.
        versions: Vec<i64>,
    },
    /// The history is inconsistent (dirty, edited or unknown migrations).
    Problems(Vec<MigrationProblem>),
}

/// Inspects the migration history.
///
/// Applied versions this binary does not know are not a problem here: the previous binary keeps
/// running against a newer schema during rolling upgrades (expand → migrate → contract).
pub async fn schema_state(pool: &PgPool) -> anyhow::Result<SchemaState> {
    let problems: Vec<_> = migrate::validate(pool)
        .await?
        .into_iter()
        .filter(|p| !matches!(p, MigrationProblem::UnknownApplied { .. }))
        .collect();
    if !problems.is_empty() {
        return Ok(SchemaState::Problems(problems));
    }
    let versions: Vec<i64> = migrate::status(pool)
        .await?
        .into_iter()
        .filter(|s| s.known && !s.applied)
        .map(|s| s.version)
        .collect();
    Ok(if versions.is_empty() {
        SchemaState::Current
    } else {
        SchemaState::Pending { versions }
    })
}

/// Fails with an actionable message unless the schema is current.
pub async fn ensure_schema_current(pool: &PgPool) -> anyhow::Result<()> {
    match schema_state(pool).await? {
        SchemaState::Current => Ok(()),
        SchemaState::Pending { versions } => bail!(
            "the database schema is not current: {} pending migration(s) ({}). \
             Run `sourcegull migrate up` first (Helm and Compose run it as a migrate job).",
            versions.len(),
            join(&versions)
        ),
        SchemaState::Problems(problems) => bail!(
            "the migration history is inconsistent ({problems:?}); \
             see `sourcegull migrate status` and `sourcegull migrate validate`"
        ),
    }
}

fn join(versions: &[i64]) -> String {
    versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The variable that holds the key-encryption keys inline, instead of a key file.
pub const INLINE_KEYS_VAR: &str = "SOURCEGULL_KEYS";

/// `SOURCEGULL_KEYS` as read by [`capture_inline_keys`]: `None` before the capture, `Some(None)`
/// when the variable was not set.
static INLINE_KEYS: OnceLock<Option<Arc<Zeroizing<String>>>> = OnceLock::new();

/// Reads `SOURCEGULL_KEYS` and removes it from the process environment, so the git processes and
/// other children Sourcegull starts never inherit it. An empty value counts as unset (Compose files
/// pass `${SOURCEGULL_KEYS:-}`). Later calls do nothing.
///
/// Secret injectors that resolve references before the process starts (and then run it with
/// the resolved environment) work unchanged: the value is read here, once, from that
/// environment.
///
/// # Safety
///
/// Changes the process environment: call it before any other thread exists (`main`, before
/// the async runtime starts).
#[allow(unsafe_code)]
pub unsafe fn capture_inline_keys() {
    if INLINE_KEYS.get().is_some() {
        return;
    }
    let value = std::env::var_os(INLINE_KEYS_VAR);
    if value.is_some() {
        // SAFETY: the caller guarantees no other thread exists, so nothing reads the
        // environment concurrently.
        unsafe { std::env::remove_var(INLINE_KEYS_VAR) };
    }
    let value = value
        .filter(|v| !v.is_empty())
        .map(|v| Arc::new(Zeroizing::new(v.to_string_lossy().into_owned())));
    let _ = INLINE_KEYS.set(value);
}

/// The value [`capture_inline_keys`] read, if `SOURCEGULL_KEYS` was set.
pub fn inline_keys() -> Option<Arc<Zeroizing<String>>> {
    INLINE_KEYS.get().cloned().flatten()
}

/// Where the key-encryption keys come from.
#[derive(Clone)]
pub enum KeySource {
    /// A key file. `explicit` when its path was set (`SOURCEGULL_KEYS_FILE`) rather than defaulted
    /// to `$SOURCEGULL_DATA_DIR/keys`.
    File { path: PathBuf, explicit: bool },
    /// The keys themselves, from `SOURCEGULL_KEYS`.
    Inline(Arc<Zeroizing<String>>),
}

impl fmt::Debug for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path, explicit } => f
                .debug_struct("File")
                .field("path", path)
                .field("explicit", explicit)
                .finish(),
            Self::Inline(_) => f.write_str("Inline(<redacted>)"),
        }
    }
}

impl KeySource {
    /// The source for a process: `SOURCEGULL_KEYS` (`inline`) or the key file. Both set is an error,
    /// so it is always clear which keys are in use.
    pub fn resolve(
        inline: Option<Arc<Zeroizing<String>>>,
        file: Option<&Path>,
        default_file: PathBuf,
    ) -> anyhow::Result<Self> {
        // An empty path (`SOURCEGULL_KEYS_FILE=`) counts as unset.
        let file = file.filter(|p| !p.as_os_str().is_empty());
        match (inline, file) {
            (Some(_), Some(path)) => bail!(
                "both {INLINE_KEYS_VAR} and SOURCEGULL_KEYS_FILE ({}) are set; set only one",
                path.display()
            ),
            (Some(keys), None) => Ok(Self::Inline(keys)),
            (None, Some(path)) => Ok(Self::File {
                path: path.to_owned(),
                explicit: true,
            }),
            (None, None) => Ok(Self::File {
                path: default_file,
                explicit: false,
            }),
        }
    }

    /// Reads and parses the keys.
    pub fn provider(&self) -> anyhow::Result<FileKeyProvider> {
        match self {
            Self::File { path, .. } => FileKeyProvider::from_path(path)
                .with_context(|| format!("loading encryption keys from {}", path.display())),
            Self::Inline(text) => parse_inline_keys(text),
        }
    }

    /// Where the keys come from, for logs and messages.
    pub fn describe(&self) -> String {
        match self {
            Self::File { path, .. } => path.display().to_string(),
            Self::Inline(_) => INLINE_KEYS_VAR.to_owned(),
        }
    }
}

/// Parses `SOURCEGULL_KEYS`. A value an injector should have replaced (`vault:…`) gets an error that
/// says so instead of a base64 error.
fn parse_inline_keys(text: &str) -> anyhow::Result<FileKeyProvider> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("{INLINE_KEYS_VAR} is set but empty");
    }
    if trimmed.starts_with("vault:") || trimmed.starts_with(">>vault:") {
        bail!(
            "{INLINE_KEYS_VAR} holds an unresolved secret reference (it starts with `vault:`): \
             the secret injector did not run for this container"
        );
    }
    FileKeyProvider::parse_inline(trimmed)
        .with_context(|| format!("loading encryption keys from {INLINE_KEYS_VAR}"))
}

/// Loads the key-encryption keys. A missing default key file is a warning (features that store
/// secrets are then unavailable); every other failure is an error.
pub fn load_keys(source: &KeySource) -> anyhow::Result<Option<FileKeyProvider>> {
    if let KeySource::File {
        path,
        explicit: false,
    } = source
        && !path.exists()
    {
        tracing::warn!(
            path = %path.display(),
            "no encryption keys; set SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS before storing secrets"
        );
        return Ok(None);
    }
    let provider = source.provider()?;
    tracing::info!(source = %source.describe(), current_key = provider.current_kek_id(), "encryption keys loaded");
    Ok(Some(provider))
}

/// Creates a key file with one random key (mode 0600) unless it exists. Returns whether the file
/// was created.
pub fn ensure_keys_file(path: &Path) -> anyhow::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut key = sourcegull_crypto::Zeroizing::new([0u8; KEY_LEN]);
    rand::rngs::OsRng.fill_bytes(key.as_mut());
    let line = sourcegull_crypto::Zeroizing::new(format!(
        "# Sourcegull key-encryption keys (id:base64). Back this file up; append a line to rotate.\n\
         k1:{}\n",
        STANDARD.encode(key.as_ref())
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    tracing::info!(path = %path.display(), "generated encryption key file");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn generates_a_loadable_private_key_file() {
        let dir = std::env::temp_dir().join(format!("sourcegull-keys-{}", std::process::id()));
        let path = dir.join("sub/keys");
        assert!(ensure_keys_file(&path).unwrap());
        assert!(!ensure_keys_file(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let file = |explicit| KeySource::File {
            path: path.clone(),
            explicit,
        };
        let provider = load_keys(&file(true)).unwrap().unwrap();
        assert_eq!(provider.current_kek_id(), "k1");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(load_keys(&file(false)).unwrap().is_none());
        assert!(load_keys(&file(true)).is_err());
    }

    fn inline(text: &str) -> KeySource {
        KeySource::Inline(Arc::new(Zeroizing::new(text.to_owned())))
    }

    #[test]
    fn loads_inline_keys() {
        let k = |b: u8| STANDARD.encode([b; KEY_LEN]);
        let provider = load_keys(&inline(&format!("k1:{},k2:{}\n", k(1), k(2))))
            .unwrap()
            .unwrap();
        assert_eq!(provider.current_kek_id(), "k2");

        for (value, expected) in [
            ("", "empty"),
            ("  ", "empty"),
            (
                "vault:secret/data/sourcegull#keys",
                "unresolved secret reference",
            ),
            (
                ">>vault:database/creds/sourcegull#keys",
                "unresolved secret reference",
            ),
            ("k1:short", "SOURCEGULL_KEYS"),
        ] {
            let err = format!("{:#}", load_keys(&inline(value)).unwrap_err());
            assert!(err.contains(expected), "{value:?}: {err}");
            assert!(
                !err.contains("secret/data"),
                "the value is not echoed: {err}"
            );
        }
        assert_eq!(format!("{:?}", inline("k1:x")), "Inline(<redacted>)");
    }

    #[test]
    fn inline_keys_and_a_key_file_conflict() {
        let keys = Some(Arc::new(Zeroizing::new("k1:x".to_owned())));
        let default = PathBuf::from("/data/keys");
        let err = KeySource::resolve(keys.clone(), Some(Path::new("/k")), default.clone())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("SOURCEGULL_KEYS") && err.contains("SOURCEGULL_KEYS_FILE"),
            "{err}"
        );
        assert!(matches!(
            KeySource::resolve(keys, None, default.clone()).unwrap(),
            KeySource::Inline(_)
        ));
        assert!(matches!(
            KeySource::resolve(None, None, default).unwrap(),
            KeySource::File {
                explicit: false,
                ..
            }
        ));
    }
}
