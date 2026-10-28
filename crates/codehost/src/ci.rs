//! CI job tokens: [`CiJobVerifier`] proves that a request comes from a CI job running for a
//! repository (SCIP uploads from GitLab CI with `CI_JOB_TOKEN`).

use async_trait::async_trait;

use crate::Result;

/// The running CI job that holds a job token.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CiJob {
    /// The external id (`ExternalRepoSpec::id`, e.g. a GitLab project id) of the repository
    /// the job runs for.
    pub repo: String,
    /// The code host's job id.
    pub id: i64,
    /// The branch or tag the job runs for.
    pub git_ref: String,
    /// The code host username of the user who triggered the job (empty when unknown).
    pub username: String,
}

/// Asks the code host which job, and so which repository, a CI job token belongs to.
#[async_trait]
pub trait CiJobVerifier: Send + Sync {
    /// The running job that holds `token`. Fails with
    /// [`CodeHostError::Unauthorized`](crate::CodeHostError::Unauthorized) when the code host
    /// does not accept the token (unknown, or the job has finished).
    async fn job(&self, token: &str) -> Result<CiJob>;
}
