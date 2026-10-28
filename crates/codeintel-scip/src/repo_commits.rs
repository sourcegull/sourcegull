//! Upload commits checked against the repository nodes.

use std::sync::Arc;

use crate::worker::{CommitChecker, CommitStatus};
use async_trait::async_trait;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::{ErrorKind, RepoClients};
use sourcegull_types::RepoId;

/// Checks upload commits against the repository nodes: resolves the commit and, when the node
/// does not have it, fetches the repository and resolves it again.
pub struct RepoCommitChecker {
    clients: Arc<RepoClients>,
}

impl RepoCommitChecker {
    pub fn new(clients: Arc<RepoClients>) -> Self {
        Self { clients }
    }

    async fn resolve(&self, repo: &pb::RepoRef, commit: &str) -> CommitStatus {
        let mut client = match self.clients.for_repo(RepoId(repo.id)) {
            Ok(c) => c,
            Err(e) => return CommitStatus::Unknown(e.to_string()),
        };
        let resp = client
            .resolve_revision(pb::ResolveRevisionRequest {
                repo: Some(repo.clone()),
                rev: commit.to_owned(),
            })
            .await;
        match resp {
            // The node peels to a commit: another oid means `commit` names a tag or tree.
            Ok(r) if r.get_ref().commit_oid == commit => CommitStatus::Exists,
            Ok(_) => CommitStatus::Missing,
            Err(status) => match ErrorKind::of(&status) {
                Some(ErrorKind::RevisionNotFound) => CommitStatus::Missing,
                Some(ErrorKind::RepoNotFound) => {
                    CommitStatus::Unknown("the repository is not cloned yet".into())
                }
                Some(ErrorKind::CloneInProgress) => {
                    CommitStatus::Unknown("the repository is being cloned".into())
                }
                _ => CommitStatus::Unknown(status.message().to_owned()),
            },
        }
    }
}

#[async_trait]
impl CommitChecker for RepoCommitChecker {
    async fn check(&self, repo: RepoId, repo_name: &str, commit: &str) -> CommitStatus {
        let r = pb::RepoRef {
            id: repo.get(),
            name: repo_name.to_owned(),
        };
        match self.resolve(&r, commit).await {
            CommitStatus::Missing => {}
            other => return other,
        }
        let fetched = match self.clients.for_repo(repo) {
            Ok(mut c) => {
                c.fetch_repo(pb::FetchRepoRequest {
                    repo: Some(r.clone()),
                })
                .await
            }
            Err(e) => return CommitStatus::Unknown(e.to_string()),
        };
        if let Err(status) = fetched {
            tracing::warn!(repo = %repo_name, error = %status.message(),
                "fetching for an upload's commit failed");
        }
        self.resolve(&r, commit).await
    }
}
