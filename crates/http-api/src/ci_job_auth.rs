//! GitLab CI job tokens for SCIP uploads.
//!
//! `gull code-intel upload` running in GitLab CI without an access token sends the job's
//! `CI_JOB_TOKEN` as `Authorization: token-gitlab-ci <token>`. When the site config enables
//! `codeIntel.gitlabJobTokenUploads`, the upload is accepted for a repository if a code host
//! connection listing it answers `GET /api/v4/job` (with `JOB-TOKEN: <token>`) with a job of
//! the repository's GitLab project. Positive checks, with the job they found, are cached for a
//! minute per token and repository ([`AppState::ci_job_tokens`]), so the parts of a multipart
//! upload cost one GitLab request.

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use sourcegull_codehost::CiJob;
use sourcegull_repos::Registry;
use sourcegull_services::AppState;
use sourcegull_types::Repo;

/// The `Authorization` scheme of a GitLab CI job token.
pub const SCHEME: &str = "token-gitlab-ci";

/// The job token of an `Authorization: token-gitlab-ci …` header.
pub(crate) fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(char::is_whitespace))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(SCHEME))
        .map(|(_, token)| token.trim().to_owned())
        .filter(|t| !t.is_empty())
}

/// The running CI job of `repo`'s GitLab project that holds `token`, or `None`. Code host
/// errors (an unknown or finished job, an unreachable instance) mean `None`; they are logged,
/// not shown.
pub(crate) async fn verify(
    state: &AppState,
    token: &str,
    repo: &Repo,
) -> Result<Option<CiJob>, sourcegull_db::DbError> {
    let key: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    if let Some(job) = state.ci_job_tokens.get(key, repo.id).await {
        return Ok(Some(job));
    }
    if repo.external_service_type != sourcegull_codehost_gitlab::SERVICE_TYPE
        || repo.external_id.is_empty()
    {
        return Ok(None);
    }
    let Some(keyring) = state.keyring.clone() else {
        tracing::warn!("CI job token upload refused: encryption keys are not configured");
        return Ok(None);
    };
    let services = state.db.external_services(Some(keyring));
    let registry = Registry::default();
    for (service_id, _) in state.db.repos().sources(repo.id).await? {
        let Some(svc) = services.get(service_id).await? else {
            continue;
        };
        let config = match services.config(service_id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, external_service = service_id.get(), "reading a connection failed");
                continue;
            }
        };
        let verifier = match registry
            .for_connection(svc.id.get())
            .ci_job_verifier(svc.kind, &config)
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, external_service = service_id.get(), "unusable connection");
                continue;
            }
        };
        match verifier.job(token).await {
            Ok(job) if job.repo == repo.external_id => {
                state.ci_job_tokens.insert(key, repo.id, job.clone()).await;
                return Ok(Some(job));
            }
            Ok(job) => {
                tracing::info!(repo = %repo.name, job_project = %job.repo, "CI job token of another project");
            }
            Err(e) => {
                tracing::info!(repo = %repo.name, error = %e, "CI job token not accepted by the code host");
            }
        }
    }
    Ok(None)
}
