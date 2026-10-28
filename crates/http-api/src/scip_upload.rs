//! SCIP index uploads: `POST /.api/scip/upload`, compatible with
//! common SCIP upload clients.
//!
//! Query parameters: `repository`, `commit`, `root`, `indexerName`,
//! `indexerVersion`, `associatedIndexId`. A single request carries the whole index (usually
//! gzip-compressed) and is answered `202 {"id": "<n>"}`. A multipart upload starts with
//! `multiPart=true&numParts=N` (no body; same answer), sends each part with
//! `uploadId=<n>&index=<i>` and finishes with `uploadId=<n>&done=true`; those answer 204.
//!
//! The caller must be signed in (an access token, `Authorization: token …`) and able to read
//! the repository; an unreadable repository answers like a missing one. Executors upload with
//! their job token instead (`Authorization: Bearer …`), for the job's repository only;
//! the upload is then associated with the job. With `codeIntel.gitlabJobTokenUploads`, a GitLab
//! CI job uploads with its `CI_JOB_TOKEN` (`Authorization: token-gitlab-ci …`) for the
//! repository of the job's GitLab project ([`crate::ci_job_auth`]); any other repository, and
//! a token GitLab does not accept, answer like a missing repository. Such uploads record the
//! GitLab job id, ref and the username of the user who triggered the job. Parts can only be
//! added by the user (or executor job, or GitLab CI job) that started the upload.
//!
//! An upload larger than `codeIntel.maxUploadBytes` (the bytes sent, summed over the parts of a
//! multipart upload) answers 413: the server stops reading the body at the limit and deletes
//! the upload with whatever it stored of it.
//! `/.api/lsif/upload` answers 410: LSIF is not supported.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use sourcegull_authn::Actor;
use sourcegull_codeintel_scip::store::{
    AddPart, CodeIntelStore, IndexJobStore, NewUpload, Upload, UploadCiJob, UploadState,
    normalize_root, upload_index_key as index_key, upload_part_key as part_key,
};
use sourcegull_db::JobTokenScope;
use sourcegull_http_extract::{MaybeActor, error_response};
use sourcegull_services::AppState;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadParams {
    repository: Option<String>,
    commit: Option<String>,
    root: Option<String>,
    indexer_name: Option<String>,
    indexer_version: Option<String>,
    associated_index_id: Option<i64>,
    multi_part: Option<String>,
    num_parts: Option<i32>,
    upload_id: Option<i64>,
    index: Option<i32>,
    done: Option<String>,
}

fn flag(v: &Option<String>) -> bool {
    v.as_deref()
        .is_some_and(|v| matches!(v, "true" | "1" | "yes"))
}

fn bad_request(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, "INVALID_INPUT", message)
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "scip upload failed");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "Internal error.",
    )
}

/// `body` as a stream that fails, setting the returned flag, once it has yielded more than
/// `limit` bytes; the blob store then stops reading it.
fn limited_stream(body: Body, limit: u64) -> (sourcegull_blobstore::ByteStream, Arc<AtomicBool>) {
    let exceeded = Arc::new(AtomicBool::new(false));
    let flag = exceeded.clone();
    let mut seen = 0u64;
    let stream = body
        .into_data_stream()
        .map_err(std::io::Error::other)
        .and_then(move |chunk| {
            seen = seen.saturating_add(chunk.len() as u64);
            futures::future::ready(if seen > limit {
                flag.store(true, Ordering::Relaxed);
                Err(std::io::Error::other("the upload is too large"))
            } else {
                Ok(chunk)
            })
        })
        .boxed();
    (stream, exceeded)
}

/// The request's `Content-Length`, when it has a valid one.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// `codeIntel.maxUploadBytes`.
fn max_upload_bytes(state: &AppState) -> u64 {
    state.config.load().value.code_intel_max_upload_bytes.max(1)
}

fn too_large(max: u64) -> Response {
    error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "PAYLOAD_TOO_LARGE",
        &format!(
            "The upload is larger than this instance accepts ({max} bytes, {}; site config \
             codeIntel.maxUploadBytes).",
            human_bytes(max)
        ),
    )
}

/// `n` bytes in binary units, e.g. `2.0 GiB`.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Deletes upload `id`, still `uploading`, with all its blobs, and answers 413.
async fn discard_too_large(
    state: &AppState,
    blobs: &sourcegull_blobstore::Blobs,
    id: i64,
    max: u64,
) -> Response {
    tracing::info!(
        upload = id,
        max,
        "refused an upload over codeIntel.maxUploadBytes"
    );
    if let Err(e) = CodeIntelStore::new(&state.db).delete_uploading(id).await {
        return internal(e);
    }
    delete_blobs(blobs, id).await;
    too_large(max)
}

/// Deletes every blob of upload `id`.
async fn delete_blobs(blobs: &sourcegull_blobstore::Blobs, id: i64) {
    match blobs.list(&format!("uploads/{id}/")).await {
        Ok(keys) => {
            for key in keys {
                if let Err(e) = blobs.delete(&key).await {
                    tracing::warn!(error = %e, key, "deleting an upload blob failed");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, upload = id, "listing upload blobs failed"),
    }
}

/// `/.api/lsif/upload`: LSIF is no longer accepted.
pub async fn lsif_gone() -> Response {
    error_response(
        StatusCode::GONE,
        "GONE",
        "LSIF uploads are not supported. Upload a SCIP index to /.api/scip/upload instead \
         (src code-intel upload with a SCIP indexer).",
    )
}

/// Who is uploading.
enum Uploader {
    User(Actor),
    /// An executor job.
    Job(JobTokenScope),
    /// A GitLab CI job's `CI_JOB_TOKEN`, checked per repository.
    CiJob(String),
}

fn unknown_repo(name: &str) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &format!("Unknown repository {name:?}."),
    )
}

/// `POST /.api/scip/upload`.
pub async fn upload(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    headers: HeaderMap,
    Query(p): Query<UploadParams>,
    body: Body,
) -> Response {
    let job = match crate::executors::job_from_bearer(&state, &headers).await {
        Ok(job) => job,
        Err(r) => return r,
    };
    let ci_token = crate::ci_job_auth::token_from_headers(&headers);
    let actor = match (job, actor, ci_token) {
        (Some(job), _, _) => Uploader::Job(job),
        (None, Some(actor), _) => Uploader::User(actor),
        (None, None, Some(token)) => {
            if !state
                .config
                .load()
                .value
                .code_intel_gitlab_job_token_uploads
            {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    "UNAUTHORIZED",
                    "GitLab CI job token uploads are disabled \
                     (site config codeIntel.gitlabJobTokenUploads).",
                );
            }
            Uploader::CiJob(token)
        }
        (None, None, None) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Uploads need an access token (Authorization: token …).",
            );
        }
    };
    let Some(blobs) = state.blobs.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            "Blob storage is not configured.",
        );
    };
    match p.upload_id {
        Some(id) => continue_upload(&state, &blobs, &actor, id, &p, &headers, body).await,
        None => start_upload(&state, &blobs, &actor, &p, &headers, body).await,
    }
}

async fn start_upload(
    state: &AppState,
    blobs: &sourcegull_blobstore::Blobs,
    actor: &Uploader,
    p: &UploadParams,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Some(name) = p.repository.as_deref().filter(|r| !r.is_empty()) else {
        return bad_request("The repository parameter is required.");
    };
    let commit = p.commit.as_deref().unwrap_or("").to_ascii_lowercase();
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return bad_request("The commit parameter must be a full 40-character commit SHA.");
    }
    let Some(root) = normalize_root(p.root.as_deref().unwrap_or("")) else {
        return bad_request("The root parameter must stay within the repository.");
    };
    let multipart = flag(&p.multi_part);
    let num_parts = if multipart {
        match p.num_parts {
            Some(n) if (1..=100_000).contains(&n) => n,
            _ => return bad_request("numParts must be between 1 and 100000."),
        }
    } else {
        1
    };
    let max = max_upload_bytes(state);
    if !multipart && content_length(headers).is_some_and(|n| n > max) {
        return too_large(max);
    }
    let mut ci_job = None;
    let repo = match actor {
        Uploader::User(actor) => match state.authz.access(Some(actor)).await {
            Ok(access) => state.db.repos().get_by_name_authorized(name, &access).await,
            Err(e) => return internal(e),
        },
        // The job token grants its job's repository only.
        #[allow(clippy::disallowed_methods)]
        Uploader::Job(job) => state
            .db
            .repos()
            .get_by_name(name)
            .await
            .map(|r| r.filter(|r| r.id == job.repo_id)),
        // A CI job token grants the repository of its GitLab project.
        #[allow(clippy::disallowed_methods)]
        Uploader::CiJob(token) => match state.db.repos().get_by_name(name).await {
            Ok(Some(r)) => match crate::ci_job_auth::verify(state, token, &r).await {
                Ok(Some(job)) => {
                    ci_job = Some(UploadCiJob {
                        job_id: job.id,
                        git_ref: job.git_ref,
                        username: job.username,
                    });
                    Ok(Some(r))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            },
            other => other,
        },
    };
    let repo = match repo {
        Ok(Some(r)) => r,
        Ok(None) => return unknown_repo(name),
        Err(e) => return internal(e),
    };
    let store = CodeIntelStore::new(&state.db);
    let id = match store
        .create_upload(&NewUpload {
            repo_id: repo.id,
            commit,
            root,
            indexer: p.indexer_name.clone().unwrap_or_default(),
            indexer_version: p.indexer_version.clone().unwrap_or_default(),
            num_parts,
            uploader: match actor {
                Uploader::User(a) => Some(a.user_id),
                Uploader::Job(_) | Uploader::CiJob(_) => None,
            },
            associated_index_id: match actor {
                Uploader::User(_) => p.associated_index_id,
                Uploader::Job(job) => Some(job.job_id),
                Uploader::CiJob(_) => None,
            },
            ci_job: ci_job.clone(),
        })
        .await
    {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    if let Some(job) = &ci_job {
        tracing::info!(
            upload = id,
            repo = %repo.name,
            gitlab_job = job.job_id,
            git_ref = %job.git_ref,
            gitlab_user = %job.username,
            "SCIP upload started with a GitLab CI job token"
        );
    }
    if let Uploader::Job(job) = actor
        && job.queue == sourcegull_executor_api::CODEINTEL_QUEUE
        && let Err(e) = IndexJobStore::new(&state.db)
            .set_associated_upload(job.job_id, id)
            .await
    {
        return internal(e);
    }
    if !multipart {
        let (stream, exceeded) = limited_stream(body, max);
        let size = match blobs.put(&index_key(id), stream).await {
            Ok(n) => n,
            Err(_) if exceeded.load(Ordering::Relaxed) => {
                return discard_too_large(state, blobs, id, max).await;
            }
            Err(e) => {
                let _ = store
                    .mark_upload_failed(id, "The upload was interrupted.")
                    .await;
                tracing::warn!(error = %e, upload = id, "storing an upload failed");
                return bad_request("The upload was interrupted.");
            }
        };
        if let Err(e) = store
            .mark_queued(id, i64::try_from(size).unwrap_or(i64::MAX))
            .await
        {
            return internal(e);
        }
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "id": id.to_string() })),
    )
        .into_response()
}

/// Whether `actor` started upload `u`.
async fn started_by(state: &AppState, u: &Upload, actor: &Uploader) -> Result<bool, Response> {
    if let Uploader::CiJob(token) = actor {
        let Some(started) = &u.ci_job else {
            return Ok(false);
        };
        if u.uploader_user_id.is_some() || u.associated_index_id.is_some() {
            return Ok(false);
        }
        // Internal: the upload's repository, checked against the token right after.
        #[allow(clippy::disallowed_methods)]
        let repo = state.db.repos().get(u.repo_id).await.map_err(internal)?;
        return match repo {
            Some(r) => Ok(crate::ci_job_auth::verify(state, token, &r)
                .await
                .map_err(internal)?
                .is_some_and(|job| job.id == started.job_id)),
            None => Ok(false),
        };
    }
    Ok(match actor {
        Uploader::User(a) => u.uploader_user_id == Some(a.user_id),
        Uploader::Job(job) => {
            u.uploader_user_id.is_none()
                && u.associated_index_id == Some(job.job_id)
                && u.repo_id == job.repo_id
        }
        Uploader::CiJob(_) => false,
    })
}

async fn continue_upload(
    state: &AppState,
    blobs: &sourcegull_blobstore::Blobs,
    actor: &Uploader,
    id: i64,
    p: &UploadParams,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let store = CodeIntelStore::new(&state.db);
    let unknown = || error_response(StatusCode::NOT_FOUND, "NOT_FOUND", "Unknown upload.");
    let upload: Upload = match store.get_upload(id).await {
        Ok(Some(u)) => match started_by(state, &u, actor).await {
            Ok(true) => u,
            Ok(false) => return unknown(),
            Err(r) => return r,
        },
        Ok(None) => return unknown(),
        Err(e) => return internal(e),
    };
    if upload.state != UploadState::Uploading {
        return bad_request("The upload is not accepting parts.");
    }
    if flag(&p.done) {
        let missing: Vec<i32> = (0..upload.num_parts)
            .filter(|i| !upload.uploaded_parts.contains(i))
            .collect();
        if !missing.is_empty() {
            return bad_request(&format!("Parts {missing:?} have not been uploaded."));
        }
        let parts: Vec<String> = (0..upload.num_parts).map(|i| part_key(id, i)).collect();
        let size = match sourcegull_blobstore::concat(blobs, &index_key(id), &parts).await {
            Ok(n) => n,
            Err(e) => return internal(e),
        };
        for part in &parts {
            if let Err(e) = blobs.delete(part).await {
                tracing::warn!(error = %e, part, "deleting an upload part failed");
            }
        }
        return match store
            .mark_queued(id, i64::try_from(size).unwrap_or(i64::MAX))
            .await
        {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(e),
        };
    }
    let index = p.index.unwrap_or(0);
    if !(0..upload.num_parts).contains(&index) {
        return bad_request(&format!(
            "index must be between 0 and {}.",
            upload.num_parts - 1
        ));
    }
    let key = part_key(id, index);
    let max = max_upload_bytes(state);
    // A part sent again replaces the earlier one, whose bytes stop counting.
    let replaced = if upload.uploaded_parts.contains(&index) {
        match blobs.size(&key).await {
            Ok(n) => n.unwrap_or(0),
            Err(e) => return internal(e),
        }
    } else {
        0
    };
    let received = u64::try_from(upload.uploaded_bytes).unwrap_or(0);
    let remaining = max.saturating_sub(received.saturating_sub(replaced));
    if content_length(headers).is_some_and(|n| n > remaining) {
        return discard_too_large(state, blobs, id, max).await;
    }
    let (stream, exceeded) = limited_stream(body, remaining);
    let size = match blobs.put(&key, stream).await {
        Ok(n) => n,
        Err(_) if exceeded.load(Ordering::Relaxed) => {
            return discard_too_large(state, blobs, id, max).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, upload = id, index, "storing an upload part failed");
            return bad_request("The upload was interrupted.");
        }
    };
    let added = i64::try_from(size)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(replaced).unwrap_or(i64::MAX));
    match store
        .add_part(id, index, added, i64::try_from(max).unwrap_or(i64::MAX))
        .await
    {
        Ok(AddPart::Added) => StatusCode::NO_CONTENT.into_response(),
        // Parts sent at the same time went past the limit together.
        Ok(AddPart::TooLarge) => discard_too_large(state, blobs, id, max).await,
        Ok(AddPart::NotAccepting) => {
            // The upload was discarded while this part was being stored.
            if let Ok(None) = store.get_upload(id).await {
                let _ = blobs.delete(&key).await;
            }
            bad_request("The upload is not accepting parts.")
        }
        Err(e) => internal(e),
    }
}
