//! Repository content over HTTP: raw files, directory archives and streaming blame.
//!
//! - `GET /{repo}[@{rev}]/-/raw/{path}[?rev=…]`: a file's bytes. Text is served as
//!   `text/plain`, common raster images with their type, other binaries as attachments; every
//!   response carries a sandboxing CSP so raw content can never run script on our origin.
//!   Files over [`RAW_MAX_BYTES`] are refused with 413.
//! - The same URL on a directory returns a plain-text listing, or an archive with
//!   `?format=zip|tar` (or `Accept: application/zip` / `application/x-tar`), streamed from the
//!   repo node's `Archive` RPC and cut off at [`ARCHIVE_MAX_BYTES`].
//! - `GET /.api/blame/{repo}[@{rev}]/-/stream/{path}`: blame as server-sent events, with
//!   `hunk` events whose data is a JSON array of
//!   `{commitID, startLine, endLine, author: {Name, Email, Date}, message, filename,
//!   commit: {parents, url}}` (lines 1-based, `endLine` exclusive), then one `done` event.
//!   Hunks are sent as the repo node finds them.
//!
//! Raw URLs have no fixed prefix (repository names contain slashes), so they are matched in the
//! router fallback ([`fallback`]) before the UI's SPA fallback.

// Helpers return the ready-made error `Response` as their `Err`, which is what handlers send.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sourcegull_authn::Actor;
use sourcegull_http_extract::{MaybeActor, error_response};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::AppState;
use sourcegull_services::git::{
    GitCallError, GitReader, RepoUrl, RepoUrlKind, is_full_oid, repo_page_url, signature_date,
};
use sourcegull_types::Repo;

/// Largest file served by the raw endpoint.
pub const RAW_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Largest archive streamed by the raw endpoint; the download is aborted beyond this.
pub const ARCHIVE_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// Entries listed for a directory in plain-text form.
pub const DIRECTORY_LISTING_LIMIT: u32 = 10_000;
/// `Content-Security-Policy` of raw responses: nothing may load or run.
pub const RAW_CSP: &str =
    "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; sandbox";

/// Router fallback: raw repository URLs, then the embedded UI.
pub async fn fallback(
    State(state): State<AppState>,
    actor: MaybeActor,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if matches!(method, Method::GET | Method::HEAD)
        && let Some(url) = RepoUrl::parse(uri.path()).filter(|u| u.kind == RepoUrlKind::Raw)
    {
        let params = Query::<RawParams>::try_from_uri(&uri)
            .map(|q| q.0)
            .unwrap_or_default();
        return raw(
            &state,
            actor.0,
            url,
            params,
            &headers,
            method == Method::HEAD,
        )
        .await;
    }
    crate::assets::fallback(method, uri, headers).await
}

/// Query parameters of the raw endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct RawParams {
    /// Revision, when the URL has no `@rev`.
    pub rev: Option<String>,
    /// `zip` or `tar` for a directory archive.
    pub format: Option<String>,
    /// Present to force `Content-Disposition: attachment`.
    pub download: Option<String>,
}

/// Requires a signed-in caller unless `auth.public` is on.
fn check_reader(state: &AppState, actor: Option<&Actor>) -> Result<(), Response> {
    if actor.is_some() || state.config.load().value.auth_public {
        Ok(())
    } else {
        Err(error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "You must be signed in.",
        ))
    }
}

/// Looks up a servable repository by name, as `actor` may read it: an unreadable
/// repository gets the same 404 as a missing one.
async fn find_repo(state: &AppState, actor: Option<&Actor>, name: &str) -> Result<Repo, Response> {
    let internal = |e: sourcegull_db::DbError| {
        tracing::error!(error = %e, "looking up repository");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "Internal error.",
        )
    };
    let access = state.authz.access(actor).await.map_err(internal)?;
    let repo = state
        .db
        .repos()
        .get_by_name_authorized(name, &access)
        .await
        .map_err(internal)?;
    match repo {
        None => Err(error_response(
            StatusCode::NOT_FOUND,
            "REPO_NOT_FOUND",
            "Repository not found.",
        )),
        Some(r) if r.blocked.is_some() => Err(error_response(
            StatusCode::FORBIDDEN,
            "REPO_BLOCKED",
            &format!(
                "Repository is blocked: {}",
                r.blocked.as_deref().unwrap_or_default()
            ),
        )),
        Some(r) => Ok(r),
    }
}

/// Maps a repo node error to an HTTP response.
pub fn git_error_response(e: &GitCallError) -> Response {
    let (status, code) = match e {
        GitCallError::RepoNotCloned => (StatusCode::NOT_FOUND, "REPO_NOT_CLONED"),
        GitCallError::CloneInProgress => (StatusCode::SERVICE_UNAVAILABLE, "CLONE_IN_PROGRESS"),
        GitCallError::RevisionNotFound(_) => (StatusCode::NOT_FOUND, "REVISION_NOT_FOUND"),
        GitCallError::PathNotFound(_) => (StatusCode::NOT_FOUND, "PATH_NOT_FOUND"),
        GitCallError::WrongEntryType(_) => (StatusCode::BAD_REQUEST, "WRONG_ENTRY_TYPE"),
        GitCallError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "TOO_LARGE"),
        GitCallError::InvalidArgument(_) | GitCallError::PatchFailed(_) => {
            (StatusCode::BAD_REQUEST, "INVALID_INPUT")
        }
        GitCallError::Unauthenticated(_)
        | GitCallError::PermissionDenied(_)
        | GitCallError::NonFastForward(_) => (StatusCode::FORBIDDEN, "FORBIDDEN"),
        GitCallError::Unavailable(m) => {
            tracing::warn!(error = %m, "repo node unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE")
        }
        GitCallError::Internal(m) => {
            tracing::error!(error = %m, "repo node call failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL")
        }
    };
    let mut resp = error_response(status, code, &e.to_string());
    if status == StatusCode::SERVICE_UNAVAILABLE {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    }
    resp
}

/// Resolves the repository and revision of a parsed URL.
async fn open(
    state: &AppState,
    actor: Option<&Actor>,
    url: &RepoUrl,
    rev_param: Option<&str>,
) -> Result<(GitReader, String), Response> {
    check_reader(state, actor)?;
    let repo = find_repo(state, actor, &url.repo).await?;
    let git = GitReader::new(state.repo_clients.clone(), &repo);
    let rev = url.rev.as_deref().or(rev_param).unwrap_or("");
    let commit = git
        .resolve_revision(rev)
        .await
        .map_err(|e| git_error_response(&e))?;
    Ok((git, commit))
}

async fn raw(
    state: &AppState,
    actor: Option<Actor>,
    url: RepoUrl,
    params: RawParams,
    headers: &HeaderMap,
    head: bool,
) -> Response {
    let (git, commit) = match open(state, actor.as_ref(), &url, params.rev.as_deref()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let immutable = url
        .rev
        .as_deref()
        .or(params.rev.as_deref())
        .is_some_and(is_full_oid);
    let entry = match git.stat(&commit, &url.rest).await {
        Ok(e) => e,
        Err(e) => return git_error_response(&e),
    };
    match pb::EntryKind::try_from(entry.kind) {
        Ok(pb::EntryKind::Directory) => {
            let format = archive_format(params.format.as_deref(), headers);
            match format {
                Some(format) => archive(&git, &commit, &url.rest, format, head).await,
                None => directory_listing(&git, &commit, &url.rest, head).await,
            }
        }
        Ok(pb::EntryKind::Submodule) => error_response(
            StatusCode::BAD_REQUEST,
            "WRONG_ENTRY_TYPE",
            "The path is a submodule.",
        ),
        _ => {
            if etag_matches(headers, &entry.oid) {
                let mut resp = StatusCode::NOT_MODIFIED.into_response();
                set_raw_headers(resp.headers_mut(), &entry.oid, immutable);
                return resp;
            }
            raw_file(
                &git,
                &commit,
                &url.rest,
                params.download.is_some(),
                immutable,
                head,
            )
            .await
        }
    }
}

fn etag_matches(headers: &HeaderMap, oid: &str) -> bool {
    let tag = format!("\"{oid}\"");
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == tag || t == "*")
        })
}

fn set_raw_headers(h: &mut HeaderMap, oid: &str, immutable: bool) {
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_CSP),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("\"{oid}\"")) {
        h.insert(header::ETAG, v);
    }
    // Responses depend on the caller's access, so only private caches may keep them.
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "private, max-age=86400"
        } else {
            "private, no-cache"
        }),
    );
}

/// Image types safe to show inline (no SVG: it can carry script).
const INLINE_IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/x-icon",
    "image/vnd.microsoft.icon",
    "image/avif",
];

/// The content type and whether to force a download, from the path and binary flag.
pub fn raw_content_type(path: &str, binary: bool) -> (&'static str, bool) {
    if !binary {
        return ("text/plain; charset=utf-8", false);
    }
    let guess = mime_guess::from_path(path).first_raw().unwrap_or("");
    match INLINE_IMAGE_TYPES.iter().find(|t| **t == guess) {
        Some(t) => (t, false),
        None => ("application/octet-stream", true),
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `attachment; filename="…"; filename*=UTF-8''…`.
fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

async fn raw_file(
    git: &GitReader,
    commit: &str,
    path: &str,
    download: bool,
    immutable: bool,
    head: bool,
) -> Response {
    let file = match git.read_file(commit, path, RAW_MAX_BYTES).await {
        Ok(f) => f,
        Err(e) => return git_error_response(&e),
    };
    if file.truncated || file.size > RAW_MAX_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "TOO_LARGE",
            &format!(
                "The file is {} bytes; the raw endpoint serves files up to {RAW_MAX_BYTES} bytes.",
                file.size
            ),
        );
    }
    let (content_type, attach) = raw_content_type(path, file.binary);
    let len = file.content.len();
    let mut resp = if head {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(file.content))
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    set_raw_headers(h, &file.oid, immutable);
    if attach || download {
        h.insert(
            header::CONTENT_DISPOSITION,
            content_disposition(base_name(path)),
        );
    }
    resp
}

async fn directory_listing(git: &GitReader, commit: &str, path: &str, head: bool) -> Response {
    let opts = sourcegull_services::git::TreeOptions {
        limit: DIRECTORY_LISTING_LIMIT,
        ..Default::default()
    };
    let (entries, truncated) = match git.list_tree(commit, path, &opts).await {
        Ok(v) => v,
        Err(e) => return git_error_response(&e),
    };
    let mut body = String::new();
    for e in &entries {
        body.push_str(&e.name);
        if e.kind == pb::EntryKind::Directory as i32 {
            body.push('/');
        }
        body.push('\n');
    }
    if truncated {
        body.push_str("…\n");
    }
    let mut resp = if head {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(body))
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_CSP),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    resp
}

/// The archive format requested by `?format=` or the `Accept` header.
pub fn archive_format(format: Option<&str>, headers: &HeaderMap) -> Option<pb::ArchiveFormat> {
    match format.map(str::to_ascii_lowercase).as_deref() {
        Some("zip") => return Some(pb::ArchiveFormat::Zip),
        Some("tar") => return Some(pb::ArchiveFormat::Tar),
        _ => {}
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("application/zip") {
        Some(pb::ArchiveFormat::Zip)
    } else if accept.contains("application/x-tar") {
        Some(pb::ArchiveFormat::Tar)
    } else {
        None
    }
}

/// `{repo-basename}-{short-oid}[-{dir path with '-' for '/'}].{ext}`.
pub fn archive_file_name(repo: &str, commit: &str, path: &str, ext: &str) -> String {
    let mut name = format!("{}-{}", base_name(repo), &commit[..commit.len().min(10)]);
    if !path.is_empty() {
        name.push('-');
        name.push_str(&path.replace('/', "-"));
    }
    name.push('.');
    name.push_str(ext);
    name
}

async fn archive(
    git: &GitReader,
    commit: &str,
    path: &str,
    format: pb::ArchiveFormat,
    head: bool,
) -> Response {
    let (content_type, ext) = match format {
        pb::ArchiveFormat::Zip => ("application/zip", "zip"),
        _ => ("application/x-tar", "tar"),
    };
    let file_name = archive_file_name(git.repo_name(), commit, path, ext);
    let mut resp = if head {
        Response::new(Body::empty())
    } else {
        let paths = if path.is_empty() {
            Vec::new()
        } else {
            vec![path.to_owned()]
        };
        let mut stream = match git.archive(commit, format, paths).await {
            Ok(s) => s,
            Err(e) => return git_error_response(&e),
        };
        // Wait for the first chunk so that an early failure is still a proper error response.
        let first = match stream.message().await {
            Ok(m) => m.map(|m| m.chunk),
            Err(e) => return git_error_response(&GitCallError::from(e)),
        };
        Response::new(Body::from_stream(limited_archive(first, stream)))
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CONTENT_DISPOSITION, content_disposition(&file_name));
    // Archives are streamed as-is; zip is already compressed and tar is cheap to fetch whole.
    h.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(RAW_CSP),
    );
    resp
}

/// Forwards archive chunks, failing the body (which aborts the download) past
/// [`ARCHIVE_MAX_BYTES`] or on a repo node error.
fn limited_archive(
    first: Option<Bytes>,
    rest: sourcegull_rpc::tonic::Streaming<pb::ArchiveResponse>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let first = futures::stream::iter(first.map(Ok));
    let rest = rest.map(|r| r.map(|m| m.chunk));
    let mut sent: u64 = 0;
    first.chain(rest).map(move |chunk| {
        let chunk = chunk.map_err(|s| {
            tracing::warn!(error = %s, "archive stream failed");
            std::io::Error::other(s.message().to_owned())
        })?;
        sent += chunk.len() as u64;
        if sent > ARCHIVE_MAX_BYTES {
            tracing::warn!(limit = ARCHIVE_MAX_BYTES, "archive exceeded the size limit");
            return Err(std::io::Error::other("archive exceeds the size limit"));
        }
        Ok(chunk)
    })
}

// ------------------------------------------------------------------ blame

/// Query parameters of the blame stream.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlameParams {
    /// Revision, when the URL has no `@rev`.
    pub rev: Option<String>,
    /// First line (1-based, inclusive).
    pub start_line: Option<u32>,
    /// Last line (1-based, inclusive).
    pub end_line: Option<u32>,
}

/// A commit signature in the blame stream (Go-style field names).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct BlameSignature {
    pub name: String,
    pub email: String,
    /// RFC 3339 with the author's offset.
    pub date: String,
}

/// The `commit` part of a streamed hunk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameHunkCommit {
    pub parents: Vec<String>,
    pub url: String,
}

/// One hunk of a `hunk` event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BlameHunkEvent {
    #[serde(rename = "commitID")]
    pub commit_id: String,
    pub start_line: u32,
    /// Exclusive.
    pub end_line: u32,
    pub author: BlameSignature,
    pub message: String,
    pub filename: String,
    pub commit: BlameHunkCommit,
}

/// `GET /.api/blame/{repo}[@{rev}]/-/stream/{path}`.
pub async fn blame_stream(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Path(rest): Path<String>,
    Query(params): Query<BlameParams>,
) -> Response {
    let Some(url) = RepoUrl::parse(&rest).filter(|u| u.kind == RepoUrlKind::Stream) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "Expected /.api/blame/{repo}[@{rev}]/-/stream/{path}.",
        );
    };
    let (git, commit) = match open(&state, actor.as_ref(), &url, params.rev.as_deref()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let mut hunks = match git
        .blame(
            &commit,
            &url.rest,
            params.start_line.unwrap_or(0),
            params.end_line.unwrap_or(0),
        )
        .await
    {
        Ok(s) => s,
        Err(e) => return git_error_response(&e),
    };
    let path = url.rest.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(8);
    tokio::spawn(async move {
        let mut parents: HashMap<String, Vec<String>> = HashMap::new();
        loop {
            let msg = match hunks.message().await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(status) => {
                    let e = GitCallError::from(status);
                    tracing::warn!(error = ?e, "blame stream failed");
                    let _ = tx
                        .send(
                            Event::default()
                                .event("error")
                                .json_data(serde_json::json!({ "message": e.to_string() }))
                                .unwrap_or_default(),
                        )
                        .await;
                    return;
                }
            };
            let mut out = Vec::with_capacity(msg.hunks.len());
            for h in msg.hunks {
                if !parents.contains_key(&h.commit_oid) {
                    let p = git
                        .get_commit(&h.commit_oid)
                        .await
                        .map(|c| c.parents)
                        .unwrap_or_default();
                    parents.insert(h.commit_oid.clone(), p);
                }
                out.push(hunk_event(git.repo_name(), &path, h, &parents));
            }
            let Ok(event) = Event::default().event("hunk").json_data(&out) else {
                return;
            };
            if tx.send(event).await.is_err() {
                return; // client went away; dropping `hunks` cancels the blame
            }
        }
        let _ = tx.send(Event::default().event("done").data("{}")).await;
    });
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).map(Ok::<_, Infallible>);
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn hunk_event(
    repo: &str,
    path: &str,
    h: pb::BlameHunk,
    parents: &HashMap<String, Vec<String>>,
) -> BlameHunkEvent {
    let author = h.author.unwrap_or_default();
    BlameHunkEvent {
        start_line: h.start_line,
        end_line: h.start_line + h.line_count,
        author: BlameSignature {
            name: author.name,
            email: author.email,
            date: signature_date(author.time, author.tz_offset_seconds),
        },
        message: h.summary,
        filename: if h.orig_path.is_empty() {
            path.to_owned()
        } else {
            h.orig_path
        },
        commit: BlameHunkCommit {
            parents: parents.get(&h.commit_oid).cloned().unwrap_or_default(),
            url: repo_page_url(repo, None, "commit", &h.commit_oid),
        },
        commit_id: h.commit_oid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        assert_eq!(
            raw_content_type("a.rs", false),
            ("text/plain; charset=utf-8", false)
        );
        assert_eq!(
            raw_content_type("a.svg", false).0,
            "text/plain; charset=utf-8"
        );
        assert_eq!(raw_content_type("a.png", true), ("image/png", false));
        assert_eq!(
            raw_content_type("a.svg", true),
            ("application/octet-stream", true)
        );
        assert_eq!(
            raw_content_type("a.bin", true),
            ("application/octet-stream", true)
        );
    }

    #[test]
    fn archive_names_and_formats() {
        assert_eq!(
            archive_file_name("gitlab.com/g/proj", "0123456789abcdef", "src/a", "zip"),
            "proj-0123456789-src-a.zip"
        );
        assert_eq!(archive_file_name("r", "abc", "", "tar"), "r-abc.tar");
        let mut h = HeaderMap::new();
        assert_eq!(archive_format(None, &h), None);
        assert_eq!(
            archive_format(Some("ZIP"), &h),
            Some(pb::ArchiveFormat::Zip)
        );
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/x-tar"),
        );
        assert_eq!(archive_format(None, &h), Some(pb::ArchiveFormat::Tar));
    }

    #[test]
    fn dates_and_etags() {
        assert_eq!(
            signature_date(1_700_000_060, 0),
            "2023-11-14T22:14:20+00:00"
        );
        assert_eq!(
            signature_date(1_700_000_060, 3600),
            "2023-11-14T23:14:20+01:00"
        );
        let mut h = HeaderMap::new();
        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"abc\", \"x\""),
        );
        assert!(etag_matches(&h, "abc"));
        assert!(!etag_matches(&h, "abd"));
    }
}
