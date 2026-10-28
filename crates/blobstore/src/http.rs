//! The `blob` role's HTTP API and its client.
//!
//! - `PUT /blobs/{key}`: writes the body (streamed); `{"size": n}`.
//! - `GET /blobs/{key}` (and `HEAD`): the content, with `Content-Length`; 404 when missing.
//! - `DELETE /blobs/{key}`: 204, also when missing.
//! - `GET /blobs?prefix=p`: `{"keys": [...]}`.
//!
//! Requests are counted in `sourcegull_blob_requests_total{method, status}` and timed in
//! `sourcegull_blob_request_duration_seconds{method}`.
//!
//! It is served on the internal listener, which trusts its callers like the repo node gRPC
//! port; that listener must not be reachable from outside the deployment.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sourcegull_types::metric_names;

use crate::{BlobStore, ByteStream, Error, Result, validate_key};

type Store = Arc<dyn BlobStore>;

/// Blob API requests served, by method and status.
pub const REQUESTS_METRIC: &str = metric_names::BLOB_REQUESTS_TOTAL;
/// Blob API request latency in seconds (until the response headers), by method.
pub const DURATION_METRIC: &str = metric_names::BLOB_REQUEST_DURATION_SECONDS;

/// The routes of the blob API, to merge into the internal router.
pub fn router(store: Store) -> axum::Router {
    axum::Router::new()
        .route("/blobs", get(list))
        .route("/blobs/{*key}", get(read).put(write).delete(remove))
        .layer(middleware::from_fn(track_metrics))
        .with_state(store)
}

async fn track_metrics(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let start = Instant::now();
    let resp = next.run(req).await;
    metrics::histogram!(DURATION_METRIC, "method" => method.clone())
        .record(start.elapsed().as_secs_f64());
    metrics::counter!(REQUESTS_METRIC, "method" => method, "status" => resp.status().as_u16().to_string())
        .increment(1);
    resp
}

fn error_response(e: Error) -> Response {
    match e {
        Error::NotFound(_) => StatusCode::NOT_FOUND.into_response(),
        Error::InvalidKey(_) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        e => {
            tracing::error!(error = %e, "blob store request failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Written {
    size: u64,
}

async fn write(State(store): State<Store>, Path(key): Path<String>, body: Body) -> Response {
    let stream = body.into_data_stream().map_err(io::Error::other).boxed();
    match store.put(&key, stream).await {
        Ok(size) => Json(Written { size }).into_response(),
        Err(e) => error_response(e),
    }
}

async fn read(State(store): State<Store>, Path(key): Path<String>) -> Response {
    let size = match store.size(&key).await {
        Ok(Some(n)) => n,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return error_response(e),
    };
    match store.get(&key).await {
        Ok(stream) => (
            [
                (header::CONTENT_LENGTH, size.to_string()),
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            ],
            Body::from_stream(stream),
        )
            .into_response(),
        Err(e) => error_response(e),
    }
}

async fn remove(State(store): State<Store>, Path(key): Path<String>) -> Response {
    match store.delete(&key).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    prefix: String,
}

#[derive(Serialize, Deserialize)]
struct Keys {
    keys: Vec<String>,
}

async fn list(State(store): State<Store>, Query(p): Query<ListParams>) -> Response {
    match store.list(&p.prefix).await {
        Ok(keys) => Json(Keys { keys }).into_response(),
        Err(e) => error_response(e),
    }
}

/// A client of a `blob` role.
#[derive(Debug, Clone)]
pub struct HttpBlobStore {
    base: url::Url,
    client: reqwest::Client,
}

impl HttpBlobStore {
    /// A client of the blob role at `base` (`http://host:port`).
    pub fn new(base: &str) -> Result<Self> {
        let mut base = url::Url::parse(base).map_err(|e| Error::Remote(e.to_string()))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Remote(e.to_string()))?;
        Ok(Self { base, client })
    }

    fn url(&self, key: &str) -> Result<url::Url> {
        validate_key(key)?;
        self.base
            .join(&format!("blobs/{key}"))
            .map_err(|e| Error::Remote(e.to_string()))
    }
}

fn remote(e: reqwest::Error) -> Error {
    Error::Remote(e.to_string())
}

fn check(resp: reqwest::Response, key: &str) -> Result<reqwest::Response> {
    match resp.status() {
        s if s.is_success() => Ok(resp),
        StatusCode::NOT_FOUND => Err(Error::NotFound(key.to_owned())),
        StatusCode::BAD_REQUEST => Err(Error::InvalidKey(key.to_owned())),
        s => Err(Error::Remote(format!("blob store answered {s}"))),
    }
}

#[async_trait::async_trait]
impl BlobStore for HttpBlobStore {
    async fn put(&self, key: &str, body: ByteStream) -> Result<u64> {
        let resp = self
            .client
            .put(self.url(key)?)
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await
            .map_err(remote)?;
        let w: Written = check(resp, key)?.json().await.map_err(remote)?;
        Ok(w.size)
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let resp = self
            .client
            .get(self.url(key)?)
            .send()
            .await
            .map_err(remote)?;
        Ok(check(resp, key)?
            .bytes_stream()
            .map_err(io::Error::other)
            .boxed())
    }

    async fn size(&self, key: &str) -> Result<Option<u64>> {
        let resp = self
            .client
            .head(self.url(key)?)
            .send()
            .await
            .map_err(remote)?;
        match check(resp, key) {
            Ok(resp) => Ok(resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let resp = self
            .client
            .delete(self.url(key)?)
            .send()
            .await
            .map_err(remote)?;
        match check(resp, key) {
            Ok(_) | Err(Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let url = self
            .base
            .join("blobs")
            .map_err(|e| Error::Remote(e.to_string()))?;
        let resp = self
            .client
            .get(url)
            .query(&[("prefix", prefix)])
            .send()
            .await
            .map_err(remote)?;
        let k: Keys = check(resp, prefix)?.json().await.map_err(remote)?;
        Ok(k.keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsBlobStore;

    #[tokio::test]
    async fn http_store_against_the_blob_api() {
        let dir = tempfile::tempdir().unwrap();
        let backing: Store = Arc::new(FsBlobStore::open(dir.path()).unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(backing)).await.unwrap();
        });
        let client: Store = Arc::new(HttpBlobStore::new(&format!("http://{addr}")).unwrap());
        crate::tests::exercise(client).await;
    }
}
