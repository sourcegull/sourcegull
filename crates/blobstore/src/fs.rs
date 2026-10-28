//! Blobs as files under a directory. Writes go to a temporary file in `<root>/.tmp` and are
//! renamed into place, so readers never see a partial blob.

use std::io;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::{BlobStore, ByteStream, Error, Result, validate_key};

#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// A store in `root` (created if missing).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join(".tmp"))?;
        Ok(Self { root })
    }

    /// The directory holding the blobs.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }
}

fn not_found(key: &str) -> impl Fn(io::Error) -> Error + '_ {
    move |e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::NotFound(key.to_owned())
        } else {
            Error::Io(e)
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, key: &str, mut body: ByteStream) -> Result<u64> {
        let dest = self.path(key)?;
        let tmp = self
            .root
            .join(".tmp")
            .join(uuid::Uuid::new_v4().to_string());
        let write = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut n = 0u64;
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                n += chunk.len() as u64;
                file.write_all(&chunk).await?;
            }
            file.sync_all().await?;
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&tmp, &dest).await?;
            Ok::<u64, io::Error>(n)
        };
        match write.await {
            Ok(n) => Ok(n),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e.into())
            }
        }
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let file = tokio::fs::File::open(self.path(key)?)
            .await
            .map_err(not_found(key))?;
        if file.metadata().await?.is_dir() {
            return Err(Error::NotFound(key.to_owned()));
        }
        Ok(tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024).boxed())
    }

    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<ByteStream> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(self.path(key)?)
            .await
            .map_err(not_found(key))?;
        if file.metadata().await?.is_dir() {
            return Err(Error::NotFound(key.to_owned()));
        }
        file.seek(io::SeekFrom::Start(range.start)).await?;
        let len = range.end.saturating_sub(range.start);
        Ok(tokio_util::io::ReaderStream::with_capacity(file.take(len), 256 * 1024).boxed())
    }

    async fn size(&self, key: &str) -> Result<Option<u64>> {
        match tokio::fs::metadata(self.path(key)?).await {
            Ok(m) if m.is_file() => Ok(Some(m.len())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match tokio::fs::remove_file(self.path(key)?).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let dir = match prefix.rfind('/') {
            Some(i) => {
                validate_key(&prefix[..i])?;
                self.root.join(&prefix[..i])
            }
            None => self.root.clone(),
        };
        let root = self.root.clone();
        let prefix = prefix.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk(&root, &dir, &mut out)?;
            out.retain(|k| k.starts_with(&prefix));
            out.sort();
            Ok(out)
        })
        .await
        .map_err(|e| Error::Io(io::Error::other(e)))?
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push(key);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn filesystem_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::open(dir.path().join("blobs")).unwrap();
        crate::tests::exercise(Arc::new(store.clone())).await;
        // No temporary files are left behind, even by the failed write.
        assert_eq!(
            std::fs::read_dir(store.root().join(".tmp"))
                .unwrap()
                .count(),
            0
        );
    }
}
