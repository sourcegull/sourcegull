//! The local step-result cache: one JSON file per step cache key in a directory, by default
//! `$XDG_CACHE_HOME/sourcegull/batch` or `~/.cache/sourcegull/batch`. Entries are [`CachedStep`]s, so a
//! result is reused only when
//! its fingerprint (image digests, environment, mounted files) still matches.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sourcegull_batches_steps::cache::CacheError;
use sourcegull_batches_steps::{CachedStep, StepCache};

use crate::env::Env;

/// The default cache directory.
pub fn default_dir(env: &dyn Env) -> PathBuf {
    let base = env
        .var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env.var("HOME").map(|h| Path::new(&h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("sourcegull").join("batch")
}

/// A [`StepCache`] in a directory. With `read` off (`-clear-cache`) lookups miss, but new
/// results are still stored.
#[derive(Debug, Clone)]
pub struct DiskStepCache {
    dir: PathBuf,
    read: bool,
}

impl DiskStepCache {
    pub fn new(dir: impl Into<PathBuf>, read: bool) -> Self {
        Self {
            dir: dir.into(),
            read,
        }
    }

    /// The file of `key`: keys are long and contain any character, so the name is a hash.
    pub fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!(
            "{}.json",
            hex::encode(Sha256::digest(key.as_bytes()))
        ))
    }
}

#[async_trait]
impl StepCache for DiskStepCache {
    async fn get(&self, key: &str) -> Result<Option<CachedStep>, CacheError> {
        if !self.read {
            return Ok(None);
        }
        match tokio::fs::read_to_string(self.file(key)).await {
            Ok(text) => CachedStep::from_json(&text)
                .map(Some)
                .map_err(|e| CacheError(format!("reading cache entry: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CacheError(format!("reading cache entry: {e}"))),
        }
    }

    async fn set(&self, key: &str, entry: &CachedStep) -> Result<(), CacheError> {
        let err = |e: std::io::Error| CacheError(format!("writing cache entry: {e}"));
        tokio::fs::create_dir_all(&self.dir).await.map_err(err)?;
        // Write then rename, so a concurrent run never reads half an entry.
        let path = self.file(key);
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        tokio::fs::write(&tmp, entry.to_json()).await.map_err(err)?;
        tokio::fs::rename(&tmp, &path).await.map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sourcegull_batches_steps::AfterStepResult;

    use super::*;

    #[tokio::test]
    async fn round_trip_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskStepCache::new(dir.path().join("c"), true);
        assert_eq!(cache.get("k").await.unwrap(), None);
        let entry = CachedStep {
            fingerprint: "f".into(),
            result: AfterStepResult {
                diff: b"diff --git a b\n".to_vec(),
                ..Default::default()
            },
        };
        cache.set("k", &entry).await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), Some(entry.clone()));
        // -clear-cache: misses, still writes.
        let clearing = DiskStepCache::new(dir.path().join("c"), false);
        assert_eq!(clearing.get("k").await.unwrap(), None);
        clearing.set("k2", &entry).await.unwrap();
        assert_eq!(cache.get("k2").await.unwrap(), Some(entry));
    }

    #[test]
    fn default_dirs() {
        let env: HashMap<String, String> = [("HOME".to_owned(), "/home/a".to_owned())].into();
        assert_eq!(
            default_dir(&env),
            Path::new("/home/a/.cache/sourcegull/batch")
        );
        let env: HashMap<String, String> = [
            ("HOME".to_owned(), "/home/a".to_owned()),
            ("XDG_CACHE_HOME".to_owned(), "/xdg".to_owned()),
        ]
        .into();
        assert_eq!(default_dir(&env), Path::new("/xdg/sourcegull/batch"));
    }
}
