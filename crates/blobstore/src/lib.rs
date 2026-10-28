//! Blob storage: SCIP uploads, code graph files and other large
//! objects that do not belong in Postgres.
//!
//! [`BlobStore`] is the interface. [`FsBlobStore`] keeps blobs in a directory (the default,
//! and the backing store of the `blob` role); [`HttpBlobStore`] talks to a `blob` role over
//! its HTTP API ([`router`]), so `web` and `worker` processes on other hosts share one store.
//! [`S3BlobStore`] keeps them in an S3-compatible bucket (MinIO, Garage, SeaweedFS, Ceph RGW,
//! Amazon S3), which replaces the `blob` role when every process can reach the bucket.
//!
//! Keys are relative paths of `/`-separated segments made of ASCII letters, digits, `.`, `_`
//! and `-`, not starting with `.` (see [`validate_key`]).

mod fs;
mod http;
mod s3;

use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};

pub use fs::FsBlobStore;
pub use http::{HttpBlobStore, router};
pub use s3::{DEFAULT_PART_SIZE, MIN_PART_SIZE, S3BlobStore, S3Config, testing};

/// A stream of blob content.
pub type ByteStream = BoxStream<'static, io::Result<Bytes>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("invalid blob key {0:?}")]
    InvalidKey(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("blob store request failed: {0}")]
    Remote(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Checks that `key` is a safe relative path.
pub fn validate_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 512
        && key.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidKey(key.to_owned()))
    }
}

/// Blob storage.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Prepares the store at startup (S3: creates the bucket when configured to).
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /// Writes `key` from `body`, replacing any blob there once complete. Returns the size.
    async fn put(&self, key: &str, body: ByteStream) -> Result<u64>;

    /// Reads `key`.
    async fn get(&self, key: &str) -> Result<ByteStream>;

    /// Reads the bytes of `key` in `range`, clipped to the blob's size (a range past the end
    /// is empty). The default reads from the start and skips; stores override it.
    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<ByteStream> {
        let body = self.get(key).await?;
        Ok(slice_stream(body, range))
    }

    /// The size of `key`, or `None` when it does not exist.
    async fn size(&self, key: &str) -> Result<Option<u64>>;

    /// Deletes `key`; deleting a missing blob succeeds.
    async fn delete(&self, key: &str) -> Result<()>;

    /// The keys starting with `prefix`, sorted.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// The bytes of `body` in `range`.
fn slice_stream(body: ByteStream, range: Range<u64>) -> ByteStream {
    let mut pos = 0u64;
    body.try_filter_map(move |chunk: Bytes| {
        let start = pos;
        pos += chunk.len() as u64;
        let from = range.start.clamp(start, pos) - start;
        let to = range.end.clamp(start, pos) - start;
        futures::future::ready(Ok(
            (from < to).then(|| chunk.slice(from as usize..to as usize))
        ))
    })
    .boxed()
}

/// Shared handle to a blob store.
pub type Blobs = Arc<dyn BlobStore>;

/// Writes `data` to `key`.
pub async fn put_bytes(store: &dyn BlobStore, key: &str, data: Bytes) -> Result<u64> {
    store
        .put(key, futures::stream::once(async move { Ok(data) }).boxed())
        .await
}

/// Reads all of `key`.
pub async fn get_bytes(store: &dyn BlobStore, key: &str) -> Result<Bytes> {
    let parts: Vec<Bytes> = store.get(key).await?.try_collect().await?;
    Ok(parts.concat().into())
}

/// Writes the concatenation of `parts`, in order, to `dest` (multipart uploads).
pub async fn concat(store: &Arc<dyn BlobStore>, dest: &str, parts: &[String]) -> Result<u64> {
    let mut streams = Vec::with_capacity(parts.len());
    for p in parts {
        streams.push(store.get(p).await?);
    }
    store
        .put(dest, futures::stream::iter(streams).flatten().boxed())
        .await
}

/// Copies `key` into the local file `path` (created or truncated).
pub async fn download(store: &dyn BlobStore, key: &str, path: &Path) -> Result<u64> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(path).await?;
    let mut body = store.get(key).await?;
    let mut n = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        n += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(n)
}

/// Writes the local file `path` to `key`.
pub async fn upload(store: &dyn BlobStore, key: &str, path: &Path) -> Result<u64> {
    let file = tokio::fs::File::open(path).await?;
    let body = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);
    store.put(key, body.boxed()).await
}

/// Opens the store named by `url`: `file:///path` (or a plain path), `http(s)://host:port`
/// for a `blob` role, or `s3://bucket[/prefix]` for an S3-compatible bucket (settings from
/// the URL's query and the `SOURCEGULL_S3_*` environment, see [`S3Config::from_url`]).
pub fn open(url: &str) -> Result<Blobs> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(Arc::new(HttpBlobStore::new(url)?));
    }
    if url.starts_with("s3://") {
        let config = S3Config::from_url(url, |k| std::env::var(k).ok())?;
        return Ok(Arc::new(S3BlobStore::new(&config)?));
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    if path.is_empty() {
        return Err(Error::Remote(format!("invalid blob store URL {url:?}")));
    }
    Ok(Arc::new(FsBlobStore::open(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for ok in ["a", "uploads/42/part-0", "graphs/1.scg", "x_y-z.v1"] {
            assert!(validate_key(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "/a", "a/", "a//b", "../a", "a/../b", ".hidden", "a b", "a\\b", "ü",
        ] {
            assert!(validate_key(bad).is_err(), "{bad}");
        }
    }

    /// Runs the same checks against any store.
    pub(crate) async fn exercise(store: Arc<dyn BlobStore>) {
        assert!(matches!(store.get("nope").await, Err(Error::NotFound(_))));
        assert_eq!(store.size("nope").await.unwrap(), None);
        store.delete("nope").await.unwrap();
        assert!(matches!(
            store.size("../etc/passwd").await,
            Err(Error::InvalidKey(_))
        ));

        let n = put_bytes(&*store, "uploads/1/part-0", Bytes::from_static(b"hello "))
            .await
            .unwrap();
        assert_eq!(n, 6);
        put_bytes(&*store, "uploads/1/part-1", Bytes::from_static(b"world"))
            .await
            .unwrap();
        put_bytes(&*store, "graphs/1", Bytes::from_static(b"g"))
            .await
            .unwrap();
        assert_eq!(store.size("uploads/1/part-1").await.unwrap(), Some(5));
        assert_eq!(
            store.list("uploads/1/").await.unwrap(),
            ["uploads/1/part-0", "uploads/1/part-1"]
        );
        assert_eq!(store.list("").await.unwrap().len(), 3);
        let parts = store.list("uploads/1/").await.unwrap();
        assert_eq!(concat(&store, "uploads/1/index", &parts).await.unwrap(), 11);
        assert_eq!(
            get_bytes(&*store, "uploads/1/index").await.unwrap(),
            &b"hello world"[..]
        );
        // Ranges, clipped to the size.
        for (range, want) in [
            (0..5, &b"hello"[..]),
            (4..7, b"o w"),
            (6..100, b"world"),
            (11..20, b""),
            (3..3, b""),
        ] {
            let got: Vec<Bytes> = store
                .get_range("uploads/1/index", range.clone())
                .await
                .unwrap()
                .try_collect()
                .await
                .unwrap();
            assert_eq!(got.concat(), want, "{range:?}");
        }
        assert!(matches!(
            store.get_range("nope", 0..1).await,
            Err(Error::NotFound(_))
        ));
        // Replacing.
        put_bytes(&*store, "graphs/1", Bytes::from_static(b"second"))
            .await
            .unwrap();
        assert_eq!(
            get_bytes(&*store, "graphs/1").await.unwrap(),
            &b"second"[..]
        );
        // A failed write leaves the old blob.
        let failing = futures::stream::iter([
            Ok(Bytes::from_static(b"partial")),
            Err(io::Error::other("client went away")),
        ])
        .boxed();
        assert!(store.put("graphs/1", failing).await.is_err());
        assert_eq!(
            get_bytes(&*store, "graphs/1").await.unwrap(),
            &b"second"[..]
        );

        // Files.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        assert_eq!(
            download(&*store, "uploads/1/index", &path).await.unwrap(),
            11
        );
        assert_eq!(upload(&*store, "copy", &path).await.unwrap(), 11);
        assert_eq!(
            get_bytes(&*store, "copy").await.unwrap(),
            &b"hello world"[..]
        );

        store.delete("uploads/1/part-0").await.unwrap();
        assert_eq!(store.size("uploads/1/part-0").await.unwrap(), None);
        assert_eq!(
            store.list("uploads/").await.unwrap(),
            ["uploads/1/index", "uploads/1/part-1"]
        );
    }
}
