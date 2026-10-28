//! CI job tokens: `CI_JOB_TOKEN` checked against `GET /api/v4/job`, implementing
//! [`CiJobVerifier`].

use std::sync::Arc;

use async_trait::async_trait;
use sourcegull_codehost::{CiJob, CiJobVerifier, Result};

use crate::client::{Auth, GitLabClient};

/// Verifies GitLab CI job tokens on one instance.
#[derive(Debug, Clone)]
pub struct GitLabCiJobs {
    client: Arc<GitLabClient>,
}

impl GitLabCiJobs {
    /// Uses `client`'s instance, connection pool and rate limiter (not its token).
    pub fn new(client: Arc<GitLabClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl CiJobVerifier for GitLabCiJobs {
    async fn job(&self, token: &str) -> Result<CiJob> {
        let job = self
            .client
            .with_auth(Auth::JobToken(token.to_owned()))
            .current_job()
            .await?;
        Ok(CiJob {
            repo: job.pipeline.project_id.to_string(),
            id: job.id,
            git_ref: job.git_ref,
            username: job.user.map(|u| u.username).unwrap_or_default(),
        })
    }
}
