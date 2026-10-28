//! A local disk cache of code graph files: graphs are fetched from the blob store on
//! first use and read through mmap afterwards.
//!
//! Entries are keyed by upload and processing time, since reprocessing an upload rewrites its
//! graph under the same blob key. The cache's weight is the files' size; evicting an entry
//! deletes its file (a graph still in use stays mapped until dropped).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::Upload;
use sourcegull_blobstore::Blobs;

use crate::graph::{CodeGraph, GraphError};

/// Default disk budget of the cache.
pub const DEFAULT_CACHE_BYTES: u64 = 10 << 30;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("upload {0} has no code graph")]
    NoGraph(i64),
    #[error(transparent)]
    Blob(#[from] sourcegull_blobstore::Error),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// (upload id, finished-at microseconds)
type Key = (i64, i64);

/// Code graph files cached on local disk.
pub struct GraphCache {
    dir: PathBuf,
    blobs: Blobs,
    graphs: moka::future::Cache<Key, Arc<CodeGraph>>,
    downloads: AtomicU64,
}

impl std::fmt::Debug for GraphCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphCache")
            .field("dir", &self.dir)
            .field("entries", &self.graphs.entry_count())
            .finish()
    }
}

fn file_name((id, version): Key) -> String {
    format!("{id}-{version}.graph")
}

impl GraphCache {
    /// A cache in `dir` (emptied first: files from an earlier run are not tracked) holding up
    /// to `max_bytes` of graph files.
    pub fn new(dir: &Path, blobs: Blobs, max_bytes: u64) -> std::io::Result<Self> {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        let evict_dir = dir.to_owned();
        let graphs = moka::future::Cache::builder()
            // Weights in KiB, so a u32 weight covers files up to 4 TiB.
            .max_capacity(max_bytes / 1024)
            .weigher(|_, g: &Arc<CodeGraph>| u32::try_from(g.size() / 1024).unwrap_or(u32::MAX))
            .eviction_listener(move |k: Arc<Key>, _, _| {
                let _ = std::fs::remove_file(evict_dir.join(file_name(*k)));
            })
            .build();
        Ok(Self {
            dir: dir.to_owned(),
            blobs,
            graphs,
            downloads: AtomicU64::new(0),
        })
    }

    /// The code graph of a completed upload.
    pub async fn get(&self, upload: &Upload) -> Result<Arc<CodeGraph>, CacheError> {
        let Some(blob) = upload.graph_key.clone() else {
            return Err(CacheError::NoGraph(upload.id));
        };
        let key = (
            upload.id,
            upload.finished_at.map_or(0, |t| t.timestamp_micros()),
        );
        self.graphs
            .try_get_with(key, async move {
                let path = self.dir.join(file_name(key));
                let tmp = self.dir.join(format!(
                    "{}.tmp-{}",
                    file_name(key),
                    self.downloads.fetch_add(1, Ordering::Relaxed)
                ));
                let fetched = sourcegull_blobstore::download(&*self.blobs, &blob, &tmp).await;
                if let Err(e) = fetched {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(CacheError::from(e));
                }
                tokio::fs::rename(&tmp, &path).await?;
                let graph = tokio::task::spawn_blocking(move || CodeGraph::open(&path))
                    .await
                    .map_err(|e| CacheError::Io(std::io::Error::other(e)))??;
                Ok(Arc::new(graph))
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => e,
                Err(e) => CacheError::Io(std::io::Error::other(e.to_string())),
            })
    }

    /// How many graphs were fetched from the blob store (tests).
    pub fn downloads(&self) -> u64 {
        self.downloads.load(Ordering::Relaxed)
    }
}
