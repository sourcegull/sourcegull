//! The cache of verified GitLab CI job tokens of SCIP uploads (`gull code-intel upload` in
//! GitLab CI, `Authorization: token-gitlab-ci <token>`).

use std::time::Duration;

use sourcegull_codehost::CiJob;
use sourcegull_types::RepoId;

/// How long a successful check is reused.
const TTL: Duration = Duration::from_secs(60);

/// Recently verified (token hash, repository) pairs and their jobs.
#[derive(Clone)]
pub struct CiJobTokenCache(moka::future::Cache<([u8; 32], RepoId), CiJob>);

impl CiJobTokenCache {
    /// The job verified for the token with SHA-256 hash `token_hash` and `repo`, if recent.
    pub async fn get(&self, token_hash: [u8; 32], repo: RepoId) -> Option<CiJob> {
        self.0.get(&(token_hash, repo)).await
    }

    /// Records a successful check.
    pub async fn insert(&self, token_hash: [u8; 32], repo: RepoId, job: CiJob) {
        self.0.insert((token_hash, repo), job).await;
    }
}

impl Default for CiJobTokenCache {
    fn default() -> Self {
        Self(
            moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(TTL)
                .build(),
        )
    }
}

impl std::fmt::Debug for CiJobTokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CiJobTokenCache")
            .field("entries", &self.0.entry_count())
            .finish()
    }
}
