//! Sending an index to `/.api/scip/upload`:
//!
//! - The index travels gzip-compressed; an uncompressed file is compressed to a temporary file
//!   first and `X-Uncompressed-Size` says how large it was.
//! - Up to the payload limit it is one request (answered `{"id": "<n>"}`). Larger indexes are
//!   split into parts of the limit's size: `multiPart=true&numParts=N` opens the upload, each
//!   part is sent with `uploadId` and `index` (concurrently, up to the concurrency limit), and
//!   `done=true` finishes it.
//! - Every request is retried on transport errors and 5xx answers (5 retries, 1 s apart);
//!   4xx answers fail at once.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::TryStreamExt;
use reqwest::StatusCode;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::env::Instance;

/// What the upload record says (the query parameters every request carries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub repo: String,
    pub commit: String,
    pub root: String,
    pub indexer: String,
    pub indexer_version: String,
    pub associated_index_id: Option<i64>,
}

/// How to send it.
#[derive(Debug, Clone)]
pub struct TransferOptions {
    /// The upload route, `/.api/scip/upload`.
    pub route: String,
    /// Indexes (compressed) larger than this go in parts of this size.
    pub max_payload_size: u64,
    /// Parts sent at once; 0 sends all at once.
    pub max_concurrency: usize,
    pub max_retries: u32,
    pub retry_interval: Duration,
    /// Accept any TLS certificate.
    pub insecure_skip_verify: bool,
    /// `-trace`: 1 logs requests and answers to stderr, 2 adds headers, 3 adds bodies.
    pub trace: u8,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            route: "/.api/scip/upload".into(),
            max_payload_size: 100 * 1000 * 1000,
            max_concurrency: 0,
            max_retries: 5,
            retry_interval: Duration::from_secs(1),
            insecure_skip_verify: false,
            trace: 0,
        }
    }
}

/// Why an upload failed.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    /// The instance answered with an error status.
    #[error("the instance answered {status}{}", detail(.body))]
    Status { status: StatusCode, body: String },
    #[error("request failed: {0}")]
    Transport(String),
    #[error("could not read the instance's answer: {0}")]
    Response(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn detail(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(" ({body})")
    }
}

/// Progress messages (one line each); the caller decides whether to show them.
pub type Report<'a> = &'a (dyn Fn(&str) + Sync);

/// The compressed index to send: the original file, or a compressed copy deleted on drop.
struct Payload {
    path: PathBuf,
    size: u64,
    uncompressed_size: Option<u64>,
    _temp: Option<tempfile::TempPath>,
}

async fn prepare(file: &Path, report: Report<'_>) -> Result<Payload, TransferError> {
    let path = file.to_owned();
    if crate::scip::is_gzip(&path)? {
        let size = tokio::fs::metadata(&path).await?.len();
        return Ok(Payload {
            path,
            size,
            uncompressed_size: None,
            _temp: None,
        });
    }
    report("Compressing index");
    let (temp, original, compressed) = tokio::task::spawn_blocking(move || {
        let mut input = std::fs::File::open(&path)?;
        let original = input.metadata()?.len();
        let temp = tempfile::NamedTempFile::new()?;
        let mut enc = flate2::write::GzEncoder::new(
            std::io::BufWriter::new(temp.as_file()),
            flate2::Compression::default(),
        );
        std::io::copy(&mut input, &mut enc)?;
        let mut out = enc.finish()?;
        std::io::Write::flush(&mut out)?;
        drop(out);
        let compressed = temp.as_file().metadata()?.len();
        Ok::<_, std::io::Error>((temp.into_temp_path(), original, compressed))
    })
    .await
    .map_err(std::io::Error::other)??;
    report(&format!(
        "Compressed the index from {:.2} MB to {:.2} MB",
        original as f64 / 1e6,
        compressed as f64 / 1e6
    ));
    Ok(Payload {
        path: temp.to_path_buf(),
        size: compressed,
        uncompressed_size: Some(original),
        _temp: Some(temp),
    })
}

/// One request's extra query parameters.
#[derive(Debug, Default, Clone, Copy)]
struct Step {
    multi_part: bool,
    num_parts: u64,
    upload_id: i64,
    index: Option<u64>,
    done: bool,
}

/// Sends the index at `file`; returns the upload id.
pub async fn upload(
    instance: &Instance,
    record: &Record,
    file: &Path,
    opts: &TransferOptions,
    report: Report<'_>,
) -> Result<i64, TransferError> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("gull/", env!("CARGO_PKG_VERSION")))
        .danger_accept_invalid_certs(opts.insecure_skip_verify)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| TransferError::Transport(e.to_string()))?;
    let client = Client {
        http,
        instance,
        record,
        opts,
    };
    let payload = prepare(file, report).await?;
    let max = opts.max_payload_size.max(1);
    if payload.size <= max {
        report("Uploading index");
        let id = client
            .send_retrying(Step::default(), Some((&payload, 0, payload.size)))
            .await?
            .ok_or_else(|| TransferError::Response("no upload id".into()))?;
        report("Upload complete");
        return Ok(id);
    }
    let parts = split(payload.size, max);
    let num_parts = parts.len() as u64;
    report(&format!("Sending the index in {num_parts} parts"));
    let id = client
        .send_retrying(
            Step {
                multi_part: true,
                num_parts,
                ..Step::default()
            },
            None,
        )
        .await?
        .ok_or_else(|| TransferError::Response("no upload id".into()))?;
    let limit = (opts.max_concurrency > 0).then_some(opts.max_concurrency);
    futures::stream::iter(parts.into_iter().enumerate().map(Ok))
        .try_for_each_concurrent(limit, |(i, (offset, len))| {
            let client = &client;
            let payload = &payload;
            async move {
                let i = i as u64;
                let step = Step {
                    upload_id: id,
                    index: Some(i),
                    ..Step::default()
                };
                client
                    .send_retrying(step, Some((payload, offset, len)))
                    .await?;
                report(&format!("Uploaded part {} of {num_parts}", i + 1));
                Ok::<_, TransferError>(())
            }
        })
        .await?;
    client
        .send_retrying(
            Step {
                upload_id: id,
                done: true,
                ..Step::default()
            },
            None,
        )
        .await?;
    report("All parts sent");
    Ok(id)
}

/// The `(offset, length)` of each part of a `size`-byte payload cut into `max`-byte parts.
fn split(size: u64, max: u64) -> Vec<(u64, u64)> {
    (0..size.div_ceil(max))
        .map(|i| (i * max, max.min(size - i * max)))
        .collect()
}

struct Client<'a> {
    http: reqwest::Client,
    instance: &'a Instance,
    record: &'a Record,
    opts: &'a TransferOptions,
}

impl Client<'_> {
    fn url(&self, step: Step) -> String {
        let r = self.record;
        let mut qs = url::form_urlencoded::Serializer::new(String::new());
        let mut add = |k: &str, v: &str| {
            if !v.is_empty() {
                qs.append_pair(k, v);
            }
        };
        add("repository", &r.repo);
        add("commit", &r.commit);
        add("root", &r.root);
        add("indexerName", &r.indexer);
        add("indexerVersion", &r.indexer_version);
        if let Some(id) = r.associated_index_id {
            add("associatedIndexId", &id.to_string());
        }
        if step.multi_part {
            add("multiPart", "true");
            add("numParts", &step.num_parts.to_string());
        }
        if step.upload_id != 0 {
            add("uploadId", &step.upload_id.to_string());
        }
        if let Some(i) = step.index {
            add("index", &i.to_string());
        }
        if step.done {
            add("done", "true");
        }
        format!("{}?{}", self.instance.url(&self.opts.route), qs.finish())
    }

    /// [`Self::send`] with retries.
    async fn send_retrying(
        &self,
        step: Step,
        body: Option<(&Payload, u64, u64)>,
    ) -> Result<Option<i64>, TransferError> {
        let mut attempt = 0;
        loop {
            match self.send(step, body).await {
                Ok(id) => return Ok(id),
                Err(e) if attempt < self.opts.max_retries && retryable(&e) => {
                    attempt += 1;
                    if self.opts.trace > 0 {
                        eprintln!("retrying (attempt {attempt}) after: {e}");
                    }
                    tokio::time::sleep(self.opts.retry_interval).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One POST; the upload id when the answer carries one.
    async fn send(
        &self,
        step: Step,
        body: Option<(&Payload, u64, u64)>,
    ) -> Result<Option<i64>, TransferError> {
        let url = self.url(step);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/x-protobuf+scip");
        if let Some(auth) = self.instance.credential.header() {
            req = req.header("authorization", auth);
        }
        match body {
            Some((payload, offset, len)) => {
                if step.upload_id == 0
                    && let Some(n) = payload.uncompressed_size
                {
                    req = req.header("x-uncompressed-size", n.to_string());
                }
                let mut f = tokio::fs::File::open(&payload.path).await?;
                f.seek(SeekFrom::Start(offset)).await?;
                let stream = tokio_util::io::ReaderStream::new(f.take(len));
                req = req
                    .header("content-length", len.to_string())
                    .body(reqwest::Body::wrap_stream(stream));
            }
            None => req = req.header("content-length", "0"),
        }
        let started = std::time::Instant::now();
        let resp = req
            .send()
            .await
            .map_err(|e| TransferError::Transport(e.without_url().to_string()))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = resp
            .text()
            .await
            .map_err(|e| TransferError::Transport(e.without_url().to_string()))?;
        if self.opts.trace > 0 {
            eprintln!(
                "POST {} -> {status} in {:?}",
                redact(&url),
                started.elapsed()
            );
            if self.opts.trace > 1 {
                for (k, v) in &headers {
                    eprintln!("  {k}: {}", v.to_str().unwrap_or("<binary>"));
                }
            }
            if self.opts.trace > 2 {
                eprintln!("  {text}");
            }
        }
        if status.as_u16() >= 300 {
            let trimmed = text.trim();
            let body = if trimmed.starts_with('<') {
                String::new()
            } else {
                trimmed.to_owned()
            };
            return Err(TransferError::Status { status, body });
        }
        if step.upload_id != 0 {
            return Ok(None);
        }
        #[derive(serde::Deserialize)]
        struct Answer {
            id: String,
        }
        let answer: Answer =
            serde_json::from_str(&text).map_err(|e| TransferError::Response(e.to_string()))?;
        answer
            .id
            .parse()
            .map(Some)
            .map_err(|e: std::num::ParseIntError| TransferError::Response(e.to_string()))
    }
}

fn retryable(e: &TransferError) -> bool {
    match e {
        TransferError::Status { status, .. } => status.is_server_error(),
        TransferError::Transport(_) => true,
        TransferError::Response(_) | TransferError::Io(_) => false,
    }
}

/// The URL without its query string (it names the repository, not secrets, but keep logs
/// short).
fn redact(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_match_src_cli() {
        let instance = Instance {
            endpoint: url::Url::parse("https://sourcegull.example.com").unwrap(),
            credential: crate::env::Credential::None,
        };
        let record = Record {
            repo: "gitlab.example.com/acme/demo".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            root: "lib/".into(),
            indexer: "scip-go".into(),
            indexer_version: "v0.1".into(),
            associated_index_id: None,
        };
        let opts = TransferOptions::default();
        let client = Client {
            http: reqwest::Client::new(),
            instance: &instance,
            record: &record,
            opts: &opts,
        };
        let base = "https://sourcegull.example.com/.api/scip/upload?repository=gitlab.example.com%2Facme%2Fdemo&commit=0123456789abcdef0123456789abcdef01234567&root=lib%2F&indexerName=scip-go&indexerVersion=v0.1";
        assert_eq!(client.url(Step::default()), base);
        assert_eq!(
            client.url(Step {
                multi_part: true,
                num_parts: 3,
                ..Step::default()
            }),
            format!("{base}&multiPart=true&numParts=3")
        );
        assert_eq!(
            client.url(Step {
                upload_id: 7,
                index: Some(0),
                ..Step::default()
            }),
            format!("{base}&uploadId=7&index=0")
        );
        assert_eq!(
            client.url(Step {
                upload_id: 7,
                done: true,
                ..Step::default()
            }),
            format!("{base}&uploadId=7&done=true")
        );
    }

    #[test]
    fn chunking() {
        assert_eq!(split(10, 4), [(0, 4), (4, 4), (8, 2)]);
        assert_eq!(split(8, 4), [(0, 4), (4, 4)]);
        assert_eq!(split(3, 4), [(0, 3)]);
        let parts = split(250_000_123, 100_000_000);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.iter().map(|p| p.1).sum::<u64>(), 250_000_123);
    }

    #[test]
    fn only_server_errors_and_transport_failures_are_retried() {
        let status = |s: u16| TransferError::Status {
            status: StatusCode::from_u16(s).unwrap(),
            body: String::new(),
        };
        assert!(retryable(&status(502)));
        assert!(retryable(&status(500)));
        assert!(!retryable(&status(404)));
        assert!(!retryable(&status(401)));
        assert!(retryable(&TransferError::Transport("reset".into())));
    }
}
