//! Results and logs of a job as downloads: the results' JSONL parts concatenated, the
//! same converted to CSV on the fly, and the log of its revision jobs as CSV.
//!
//! Every revision job writes its results to one blob, [`blob_key`], one JSON object per line
//! in the stream API's match shapes (`content` with `chunkMatches`, `path`, `commit`). The
//! CSV has the columns `repository`, `revision` (the commit), `file_path`, `match_count`,
//! `first_match_url`.

use std::io;

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt;
use serde_json::Value;
use sourcegull_blobstore::{Blobs, ByteStream};
use tokio::sync::mpsc;

use crate::store::SearchJobStore;

/// The blob of revision job `revision_job` of job `job`.
pub fn blob_key(job: i64, revision_job: i64) -> String {
    format!("{}{revision_job}.jsonl", job_prefix(job))
}

/// The prefix of every blob of job `job`.
pub fn job_prefix(job: i64) -> String {
    format!("{BLOB_ROOT}/{job}/")
}

/// The directory of all search job blobs.
pub const BLOB_ROOT: &str = "search-jobs";

/// Header of the results CSV.
pub const RESULTS_CSV_HEADER: [&str; 5] = [
    "repository",
    "revision",
    "file_path",
    "match_count",
    "first_match_url",
];

/// Header of the log CSV.
pub const LOG_CSV_HEADER: [&str; 6] = [
    "repository",
    "revision",
    "started_at",
    "finished_at",
    "status",
    "failure_message",
];

/// Log rows read per query.
const LOG_PAGE: i64 = 10_000;

/// Bytes buffered before a chunk is sent.
const CHUNK: usize = 256 * 1024;

/// A stream fed by a task: the task sends chunks, an `Err` ends the stream with it.
fn channel_stream() -> (mpsc::Sender<io::Result<Bytes>>, ByteStream) {
    let (tx, mut rx) = mpsc::channel::<io::Result<Bytes>>(4);
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed();
    (tx, stream)
}

/// The results of job `id`: its parts concatenated in revision job order.
pub fn results_jsonl(store: SearchJobStore, blobs: Blobs, id: i64) -> ByteStream {
    let (tx, stream) = channel_stream();
    tokio::spawn(async move {
        if let Err(e) = copy_parts(&store, &blobs, id, |chunk| {
            let tx = tx.clone();
            async move { tx.send(Ok(chunk)).await.is_ok() }
        })
        .await
        {
            let _ = tx.send(Err(e)).await;
        }
    });
    stream
}

/// The results of job `id` as CSV (see the module docs). `external_url` makes the match
/// links absolute.
pub fn results_csv(
    store: SearchJobStore,
    blobs: Blobs,
    id: i64,
    external_url: Option<String>,
) -> ByteStream {
    let (tx, stream) = channel_stream();
    tokio::spawn(async move {
        let base = external_url.unwrap_or_default();
        let base = base.trim_end_matches('/');
        let mut out = BytesMut::new();
        write_csv_row(&mut out, &RESULTS_CSV_HEADER);
        let mut pending = Vec::new();
        let res = copy_parts(&store, &blobs, id, |chunk| {
            pending.extend_from_slice(&chunk);
            // Complete lines only; parts always end with a newline.
            let mut start = 0;
            while let Some(n) = pending[start..].iter().position(|&b| b == b'\n') {
                let line = &pending[start..start + n];
                start += n + 1;
                if let Ok(v) = serde_json::from_slice::<Value>(line)
                    && let Some(row) = csv_row(&v, base)
                {
                    write_csv_row(&mut out, &row);
                }
            }
            pending.drain(..start);
            let full = (out.len() >= CHUNK).then(|| out.split().freeze());
            let tx = tx.clone();
            async move {
                match full {
                    Some(chunk) => tx.send(Ok(chunk)).await.is_ok(),
                    None => true,
                }
            }
        })
        .await;
        match res {
            Ok(()) => {
                let _ = tx.send(Ok(out.freeze())).await;
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
            }
        }
    });
    stream
}

/// Feeds the parts of job `id` to `sink` chunk by chunk; stops when it returns false.
async fn copy_parts<F, Fut>(
    store: &SearchJobStore,
    blobs: &Blobs,
    id: i64,
    mut sink: F,
) -> io::Result<()>
where
    F: FnMut(Bytes) -> Fut,
    Fut: Future<Output = bool>,
{
    let parts = store.result_parts(id).await.map_err(io::Error::other)?;
    for part in parts {
        let mut body = match blobs.get(&blob_key(id, part)).await {
            Ok(b) => b,
            // Deleted with its job meanwhile.
            Err(sourcegull_blobstore::Error::NotFound(_)) => continue,
            Err(e) => return Err(io::Error::other(e)),
        };
        while let Some(chunk) = body.next().await {
            if !sink(chunk?).await {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The CSV row of one result, or `None` for other shapes.
pub fn csv_row(v: &Value, base: &str) -> Option<[String; 5]> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
    match s("type") {
        "content" | "path" => {
            let (repo, commit, path) = (s("repository"), s("commit"), s("path"));
            let chunks = v.get("chunkMatches").and_then(Value::as_array);
            let ranges = chunks
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("ranges").and_then(Value::as_array))
                .flatten();
            let mut count = 0usize;
            let mut first: Option<(u64, u64)> = None;
            for r in ranges {
                count += 1;
                let line = |end: &str| {
                    r.get(end)
                        .and_then(|p| p.get("line"))
                        .and_then(Value::as_u64)
                        .unwrap_or_default()
                };
                let (start, end) = (line("start"), line("end"));
                if first.is_none_or(|(s, _)| start < s) {
                    first = Some((start, end));
                }
            }
            if count == 0 {
                count = v
                    .get("pathMatches")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
            }
            let mut url = format!("{base}/{repo}@{commit}/-/blob/{path}");
            match first {
                Some((s, e)) if e > s => url.push_str(&format!("?L{}-{}", s + 1, e + 1)),
                Some((s, _)) => url.push_str(&format!("?L{}", s + 1)),
                None => {}
            }
            Some([
                repo.to_owned(),
                commit.to_owned(),
                path.to_owned(),
                count.to_string(),
                url,
            ])
        }
        "commit" => {
            let count = v
                .get("ranges")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Some([
                s("repository").to_owned(),
                s("oid").to_owned(),
                String::new(),
                count.to_string(),
                format!("{base}{}", s("url")),
            ])
        }
        _ => None,
    }
}

/// The log of job `id`: one row per revision job.
pub fn log_csv(store: SearchJobStore, id: i64) -> ByteStream {
    let (tx, stream) = channel_stream();
    tokio::spawn(async move {
        let mut out = BytesMut::new();
        write_csv_row(&mut out, &LOG_CSV_HEADER);
        let mut after = 0;
        loop {
            let rows = match store.log_rows(id, after, LOG_PAGE).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(Err(io::Error::other(e))).await;
                    return;
                }
            };
            for r in &rows {
                write_csv_row(
                    &mut out,
                    &[
                        r.repository.as_str(),
                        &r.revision,
                        &time_or_null(r.started_at),
                        &time_or_null(r.finished_at),
                        &r.state,
                        r.failure_message.as_deref().unwrap_or_default(),
                    ],
                );
            }
            if out.len() >= CHUNK && tx.send(Ok(out.split().freeze())).await.is_err() {
                return;
            }
            match rows.last() {
                Some(last) if rows.len() as i64 == LOG_PAGE => after = last.id,
                _ => break,
            }
        }
        let _ = tx.send(Ok(out.freeze())).await;
    });
    stream
}

fn time_or_null(t: Option<DateTime<Utc>>) -> String {
    t.map_or_else(
        || "NULL".to_owned(),
        |t| t.to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// Appends one CSV record (RFC 4180: fields with commas, quotes or line breaks are quoted).
pub fn write_csv_row<S: AsRef<str>>(out: &mut BytesMut, fields: &[S]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b",");
        }
        let f = f.as_ref();
        if f.contains([',', '"', '\n', '\r']) {
            out.extend_from_slice(b"\"");
            out.extend_from_slice(f.replace('"', "\"\"").as_bytes());
            out.extend_from_slice(b"\"");
        } else {
            out.extend_from_slice(f.as_bytes());
        }
    }
    out.extend_from_slice(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn csv_escaping() {
        let mut out = BytesMut::new();
        write_csv_row(&mut out, &["a", "b,c", "say \"hi\"", "x\ny", ""]);
        assert_eq!(&out[..], b"a,\"b,c\",\"say \"\"hi\"\"\",\"x\ny\",\n");
    }

    #[test]
    fn csv_rows() {
        let content = json!({
            "type": "content", "path": "src/a.go", "repositoryID": 1,
            "repository": "gitlab.example.com/acme/demo", "commit": "abc",
            "chunkMatches": [
                {"content": "x", "contentStart": {"offset": 0, "line": 9, "column": 0},
                 "ranges": [
                    {"start": {"offset": 0, "line": 12, "column": 0}, "end": {"offset": 1, "line": 12, "column": 1}},
                    {"start": {"offset": 0, "line": 10, "column": 0}, "end": {"offset": 1, "line": 11, "column": 1}}
                 ]}
            ]
        });
        assert_eq!(
            csv_row(&content, "https://sourcegull.example.com").unwrap(),
            [
                "gitlab.example.com/acme/demo",
                "abc",
                "src/a.go",
                "2",
                "https://sourcegull.example.com/gitlab.example.com/acme/demo@abc/-/blob/src/a.go?L11-12"
            ]
        );
        let path = json!({
            "type": "path", "path": "a.go", "repository": "r", "commit": "c",
            "pathMatches": [{"start": {"offset": 0, "line": 0, "column": 0}, "end": {"offset": 1, "line": 0, "column": 1}}]
        });
        assert_eq!(
            csv_row(&path, "").unwrap(),
            ["r", "c", "a.go", "1", "/r@c/-/blob/a.go"]
        );
        let commit = json!({
            "type": "commit", "repository": "r", "oid": "c1", "url": "/r/-/commit/c1",
            "ranges": [[1, 2, 3]]
        });
        assert_eq!(
            csv_row(&commit, "https://s").unwrap(),
            ["r", "c1", "", "1", "https://s/r/-/commit/c1"]
        );
        assert!(csv_row(&json!({"type": "repo"}), "").is_none());
    }

    #[test]
    fn keys() {
        assert_eq!(blob_key(7, 42), "search-jobs/7/42.jsonl");
        assert!(sourcegull_blobstore::validate_key(&blob_key(7, 42)).is_ok());
        assert!(blob_key(7, 42).starts_with(&job_prefix(7)));
    }
}
