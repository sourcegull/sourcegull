//! Step-result caching.
//!
//! The result of steps `0..=i` in a workspace is stored under
//! [`sourcegull_batches::cache_key::step_cache_key`] — the key the resolver can compute, so its
//! cache-hit lookups in `batch_spec_execution_cache_entries` find what the runner stored — in a
//! [`CachedStep`] that also holds a **fingerprint** of what only the runner knows, for steps
//! `0..=i`: the image digest (so a moved tag invalidates the entry), the values of the step's
//! environment after pass-through variables are filled in (executor secrets or local
//! environment), and the contents of mounted files. An entry counts as a hit only when its
//! fingerprint matches; otherwise the step runs again and the entry is replaced. Together,
//! key and fingerprint are the cache key of the plan: repository, base commit, workspace path,
//! step definitions including env and file contents up to step i, and image digests.
//!
//! On a run the runner looks up the keys from the last step backwards and resumes after the
//! last hit by applying its diff.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::result::AfterStepResult;

/// Format version of [`StepFingerprint`]; bump when its input changes.
pub const FINGERPRINT_VERSION: u32 = 1;

/// What the runner adds to the key of one step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepInputs {
    /// From [`crate::ContainerRuntime::resolve_image`].
    pub image_digest: String,
    /// The step's environment with pass-through values filled in, before templating.
    pub env: BTreeMap<String, String>,
    /// Each `mount` path as written, with [`hash_path`] of its contents.
    pub mounts: Vec<(String, String)>,
}

/// The cache key of one step's result: where it is stored and the fingerprint the stored
/// entry must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCacheKey {
    /// [`sourcegull_batches::cache_key::step_cache_key`].
    pub key: String,
    /// Hex SHA-256 over [`StepInputs`] of steps `0..=i`.
    pub fingerprint: String,
}

/// The fingerprint of steps `0..=step`.
pub fn fingerprint(inputs: &[StepInputs], step: usize) -> String {
    let steps = &inputs[..(step + 1).min(inputs.len())];
    let input = json!({ "version": FINGERPRINT_VERSION, "steps": steps });
    hex::encode(Sha256::digest(input.to_string().as_bytes()))
}

/// What a [`StepCache`] stores under a [`StepCacheKey::key`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedStep {
    pub fingerprint: String,
    pub result: AfterStepResult,
}

impl CachedStep {
    /// The JSON form, the value of a `batch_spec_execution_cache_entries` row.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// A cache failure. The runner treats a failed lookup as a miss and a failed store as a
/// warning; neither fails the run.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CacheError(pub String);

/// Where step results are stored: the server's `batch_spec_execution_cache_entries` (through
/// the executor API, or the step cache results of a job), a local directory for `gull bulk`,
/// or [`MemoryStepCache`].
#[async_trait]
pub trait StepCache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CachedStep>, CacheError>;
    async fn set(&self, key: &str, entry: &CachedStep) -> Result<(), CacheError>;
}

/// An in-memory [`StepCache`] (tests, one-off runs). Entries are kept as JSON, so what it
/// returns went through serialization as with a real store.
#[derive(Debug, Default)]
pub struct MemoryStepCache {
    entries: Mutex<HashMap<String, String>>,
}

impl MemoryStepCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.lock().keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn remove(&self, key: &str) -> bool {
        self.lock().remove(key).is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl StepCache for MemoryStepCache {
    async fn get(&self, key: &str) -> Result<Option<CachedStep>, CacheError> {
        let text = self.lock().get(key).cloned();
        text.map(|t| CachedStep::from_json(&t).map_err(|e| CacheError(e.to_string())))
            .transpose()
    }

    async fn set(&self, key: &str, entry: &CachedStep) -> Result<(), CacheError> {
        self.lock().insert(key.to_owned(), entry.to_json());
        Ok(())
    }
}

/// Hex SHA-256 of a file or directory tree: relative paths in sorted order, the executable
/// bit, file contents and symlink targets (not times or owners, so a fresh checkout of the
/// same files hashes the same).
pub fn hash_path(path: &Path) -> std::io::Result<String> {
    let mut h = Sha256::new();
    hash_into(path, "", &mut h)?;
    Ok(hex::encode(h.finalize()))
}

fn hash_into(path: &Path, rel: &str, h: &mut Sha256) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        h.update(format!("l {rel}\0{}\0", target.display()).as_bytes());
    } else if meta.is_dir() {
        h.update(format!("d {rel}\0").as_bytes());
        let mut names: Vec<_> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        for name in names {
            let child_rel = if rel.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{rel}/{}", name.to_string_lossy())
            };
            hash_into(&path.join(&name), &child_rel, h)?;
        }
    } else {
        let content = std::fs::read(path)?;
        h.update(format!("f {rel}\0{}\0{}\0", executable(&meta), content.len()).as_bytes());
        h.update(&content);
    }
    Ok(())
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_cover_earlier_steps_only() {
        let a = StepInputs {
            image_digest: "sha256:1".into(),
            ..Default::default()
        };
        let b = StepInputs {
            image_digest: "sha256:2".into(),
            ..Default::default()
        };
        let secret = StepInputs {
            env: [("TOKEN".to_owned(), "v2".to_owned())].into(),
            ..a.clone()
        };
        assert_eq!(
            fingerprint(&[a.clone(), a.clone()], 0),
            fingerprint(&[a.clone(), b.clone()], 0)
        );
        assert_ne!(
            fingerprint(&[a.clone(), a.clone()], 1),
            fingerprint(&[a.clone(), b], 1)
        );
        assert_ne!(
            fingerprint(std::slice::from_ref(&a), 0),
            fingerprint(&[secret], 0)
        );
    }

    #[test]
    fn path_hashes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("d/e")).unwrap();
        std::fs::write(dir.path().join("d/e/f"), "1").unwrap();
        let before = hash_path(&dir.path().join("d")).unwrap();
        assert_eq!(before, hash_path(&dir.path().join("d")).unwrap());
        std::fs::write(dir.path().join("d/e/f"), "2").unwrap();
        assert_ne!(before, hash_path(&dir.path().join("d")).unwrap());
        assert!(hash_path(&dir.path().join("missing")).is_err());
    }
}
