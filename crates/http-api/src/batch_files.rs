//! Bulk change spec workspace files: the files a server-side batch
//! spec's steps `mount`.
//!
//! - `POST /.api/files/batch-changes/{batchSpecID}`: uploads one file as a command-line
//!   client's remote run sends it (`UploadBatchSpecWorkspaceFiles`): a
//!   `multipart/form-data` body with the fields `filepath` (the file's directory relative to
//!   the bulk change spec's directory, `""` at its root), `filemod` (its modification time, Go's
//!   `time.Time.String()` format, e.g. `2022-06-23 18:44:55.123 +0000 UTC`; RFC 3339 works
//!   too) and `file` (the content, its part's file name is the file name). Files over
//!   [`MAX_FILE_BYTES`] are refused with 413. Only the spec's creator
//!   (or a site admin) may upload, to a server-side spec (`createBatchSpecFromRaw`) that has
//!   not been executed yet; uploading the same path and name again replaces the file. Answers
//!   `200 {"id": "<file id>"}`.
//! - `GET`/`HEAD /.api/files/batch-changes/{batchSpecID}/{file}`: downloads a file, named by
//!   its id (`BatchSpecWorkspaceFile.url`) or its path relative to the bulk change spec's directory
//!   (`dir/name`), for those who may administer the spec's namespace.
//! - `GET`/`HEAD /.executors/files/batch-changes/{batchSpecID}/{file}`: the same for an
//!   executor, authenticated with the job token (`Authorization: Bearer …`) of a running
//!   `batches` job of that spec.
//!
//! `batchSpecID` is the spec's GraphQL ID (or its random id). Uploads and downloads need batch
//! changes to be enabled for the caller, uploads `BATCH_CHANGES#WRITE`.

// Helpers return the ready-made error `Response` as their `Err`, which is what handlers send.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDateTime, Utc};
use sourcegull_authn::access::require_permission;
use sourcegull_authn::{Actor, Permission};
use sourcegull_batches::store::{
    BatchSpec, BatchSpecStore, BatchSpecWorkspaceFile, BatchSpecWorkspaceFileStore,
    NewBatchSpecWorkspaceFile,
};
use sourcegull_http_extract::{MaybeActor, access_error_response, error_response};
use sourcegull_services::AppState;
use sourcegull_services::batch_files::MAX_FILE_BYTES;
use sourcegull_services::batches::access::{self, Unavailable};
use sourcegull_services::relay::unmarshal_id;

use crate::repo_content::RAW_CSP;

/// Body limit of the upload route: one file plus the multipart framing and fields.
pub const UPLOAD_BODY_LIMIT: usize = MAX_FILE_BYTES + 64 * 1024;

fn bad_request(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, "INVALID_INPUT", message)
}

fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "Bulk change spec or file not found.",
    )
}

fn forbidden(message: &str) -> Response {
    error_response(StatusCode::FORBIDDEN, "FORBIDDEN", message)
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "bulk change spec workspace files");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Internal error.",
    )
}

/// The random id of a spec from a GraphQL ID (`BatchSpec:"<rand>"`) or the random id itself.
pub fn spec_rand_id(id: &str) -> Option<String> {
    if let Some((kind, key)) = unmarshal_id(id)
        && kind == "BatchSpec"
    {
        return serde_json::from_str::<String>(&key).ok();
    }
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
}

async fn load_spec(state: &AppState, id: &str) -> Result<BatchSpec, Response> {
    let rand = spec_rand_id(id).ok_or_else(not_found)?;
    BatchSpecStore::new(&state.db)
        .get_by_rand_id(&rand)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)
}

/// Signed in, with bulk changes enabled for them (`batchChanges.enabled`,
/// `batchChanges.restrictToAdmins`).
fn require_user(state: &AppState, actor: Option<Actor>) -> Result<Actor, Response> {
    let actor = actor.ok_or_else(|| {
        error_response(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "You must be signed in.",
        )
    })?;
    match access::available(&state.config.load().value, &actor) {
        Ok(()) => Ok(actor),
        Err(Unavailable::Disabled) => Err(forbidden("Bulk changes are disabled.")),
        Err(Unavailable::DisabledForUser) => Err(forbidden(
            "Bulk changes are disabled for non-site-admin users.",
        )),
    }
}

/// A directory relative to the bulk change spec's directory, cleaned (`./a/b/` → `a/b`); `None` when
/// it leaves that directory or is absolute.
pub fn clean_dir(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// A modification time as clients send it (Go's `Time.String()`:
/// `2006-01-02 15:04:05.999999999 -0700 MST`), or RFC 3339.
pub fn parse_filemod(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    // Go appends a monotonic clock reading (" m=+0.1") to local times; it is not part of the
    // wall time.
    let s = s.split(" m=").next().unwrap_or(s);
    let mut fields = s.split(' ');
    let (date, time, offset) = (fields.next()?, fields.next()?, fields.next()?);
    let local =
        NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M:%S%.f").ok()?;
    let offset = chrono::FixedOffset::east_opt(parse_offset(offset)?)?;
    Some(
        local
            .and_local_timezone(offset)
            .single()?
            .with_timezone(&Utc),
    )
}

/// `+0000` / `-0700` in seconds east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// `POST /.api/files/batch-changes/{batchSpecID}`.
pub async fn upload(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    Path(spec_id): Path<String>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Response {
    let actor = match require_user(&state, actor) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if let Err(e) = require_permission(&actor, Permission::BatchChangesWrite) {
        return access_error_response(&e);
    }
    let spec = match load_spec(&state, &spec_id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    // Someone else's spec is "not found", as its ID would be secret to them.
    if !access::is_creator_or_site_admin(&actor, spec.user_id) {
        return not_found();
    }
    if !spec.created_from_raw {
        return bad_request(
            "Files can only be uploaded for bulk change specs executed server-side.",
        );
    }
    if spec.execution_requested_at.is_some() {
        return error_response(
            StatusCode::CONFLICT,
            "CONFLICT",
            "The bulk change spec has been executed; its files can no longer change.",
        );
    }
    let Ok(mut multipart) = multipart else {
        return bad_request("Expected a multipart/form-data body.");
    };
    let mut dir: Option<String> = None;
    let mut modified: Option<String> = None;
    let mut file: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return multipart_error(e),
        };
        match field.name().unwrap_or_default() {
            "filepath" => match field.text().await {
                Ok(t) => dir = Some(t),
                Err(e) => return multipart_error(e),
            },
            "filemod" => match field.text().await {
                Ok(t) => modified = Some(t),
                Err(e) => return multipart_error(e),
            },
            "file" => {
                let name = field.file_name().unwrap_or_default().to_owned();
                let mut content = Vec::new();
                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if content.len() + chunk.len() > MAX_FILE_BYTES {
                                return too_large();
                            }
                            content.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(e) => return multipart_error(e),
                    }
                }
                file = Some((name, content));
            }
            _ => {}
        }
    }
    let Some((name, content)) = file else {
        return bad_request("The file field is missing.");
    };
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return bad_request("The file name is invalid.");
    }
    let Some(dir) = clean_dir(dir.as_deref().unwrap_or("")) else {
        return bad_request("The filepath must stay within the bulk change spec's directory.");
    };
    let modified_at = match modified.as_deref() {
        None | Some("") => Utc::now(),
        Some(m) => match parse_filemod(m) {
            Some(t) => t,
            None => return bad_request("The filemod field is not a valid time."),
        },
    };
    match BatchSpecWorkspaceFileStore::new(&state.db)
        .upsert(&NewBatchSpecWorkspaceFile {
            batch_spec_id: spec.id,
            path: &dir,
            filename: &name,
            content: &content,
            modified_at,
        })
        .await
    {
        Ok(f) => Json(serde_json::json!({ "id": f.rand_id })).into_response(),
        Err(e) => internal(e),
    }
}

fn too_large() -> Response {
    error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "TOO_LARGE",
        &format!("The file exceeds the limit of {MAX_FILE_BYTES} bytes."),
    )
}

fn multipart_error(e: axum::extract::multipart::MultipartError) -> Response {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return too_large();
    }
    bad_request(&format!("Invalid multipart body: {}", e.body_text()))
}

/// The file `name` of `spec`: its id, else its path relative to the spec's directory.
async fn find_file(
    state: &AppState,
    spec: &BatchSpec,
    name: &str,
) -> Result<BatchSpecWorkspaceFile, Response> {
    let store = BatchSpecWorkspaceFileStore::new(&state.db);
    if let Some(f) = store
        .get_by_rand_id(spec.id, name)
        .await
        .map_err(internal)?
    {
        return Ok(f);
    }
    let full = clean_dir(name).ok_or_else(not_found)?;
    let (dir, file) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
    store
        .get_by_name(spec.id, dir, file)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)
}

async fn send_file(state: &AppState, f: &BatchSpecWorkspaceFile, head: bool) -> Response {
    let body = if head {
        Body::empty()
    } else {
        match BatchSpecWorkspaceFileStore::new(&state.db)
            .content(f.id)
            .await
        {
            Ok(Some(c)) => Body::from(c),
            Ok(None) => return not_found(),
            Err(e) => return internal(e),
        }
    };
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(f.size.max(0) as u64),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        f.filename.replace(['"', '\\', '\r', '\n'], "_")
    )) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(
        &f.modified_at
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
    ) {
        h.insert(header::LAST_MODIFIED, v);
    }
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

/// `GET`/`HEAD /.api/files/batch-changes/{batchSpecID}/{file}`.
pub async fn download(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    method: Method,
    Path((spec_id, name)): Path<(String, String)>,
) -> Response {
    let actor = match require_user(&state, actor) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let spec = match load_spec(&state, &spec_id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match access::can_see_spec_details(&state.db, Some(&actor), &spec).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return internal(e),
    }
    match find_file(&state, &spec, &name).await {
        Ok(f) => send_file(&state, &f, method == Method::HEAD).await,
        Err(r) => r,
    }
}

/// `GET`/`HEAD /.executors/files/batch-changes/{batchSpecID}/{file}` (job token).
pub async fn executor_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: Method,
    Path((spec_id, name)): Path<(String, String)>,
) -> Response {
    let job = match crate::executors::batches_job_for(&state, &headers).await {
        Ok(j) => j,
        Err(r) => return r,
    };
    if spec_rand_id(&spec_id).as_deref() != Some(job.spec.rand_id.as_str()) {
        return error_response(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "The job token is not for a job of this bulk change spec.",
        );
    }
    match find_file(&state, &job.spec, &name).await {
        Ok(f) => send_file(&state, &f, method == Method::HEAD).await,
        Err(r) => r,
    }
}
