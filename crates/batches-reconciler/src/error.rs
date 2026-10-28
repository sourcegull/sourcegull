//! Reconciler errors and how they map to the queue: [`ReconcileError::is_retryable`] errors
//! leave the changeset `ERRORED` and are retried with backoff (pgqueue, `FAILED` after the
//! queue's `max_failures`); the others fail it at once.

use sourcegull_codehost::CodeHostError;
use sourcegull_db::DbError;
use sourcegull_pgqueue::JobError;
use sourcegull_services::git::GitCallError;

/// A failed reconciliation or sync.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// Neither the user who last applied the bulk change nor the site has a credential for
    /// the code host.
    #[error(
        "no credential to change merge requests in {repo}: {who} no {service_type} credential \
         for {service_id} and there is no site credential; add an access token with the api \
         scope under Batch Changes in your user settings (or ask a site admin to add a global one)"
    )]
    MissingCredentials {
        repo: String,
        /// "the user who last applied the bulk change has" or similar.
        who: String,
        service_type: String,
        service_id: String,
    },
    /// Another changeset already uses the branch.
    #[error("cannot create changeset on the same branch in multiple bulk changes")]
    SameBranch,
    /// The repository is archived or refuses the push: the changeset is now read-only.
    #[error("cannot push to {0}: the repository is archived or read-only")]
    ReadOnly(String),
    /// The spec's patch does not apply to its base commit.
    #[error("creating commit from patch for repository {repo}: {message}")]
    PatchFailed { repo: String, message: String },
    /// `batchChanges.rejectUnverifiedCommit` is set but the commit cannot be created through
    /// the code host's API.
    #[error(
        "cannot create a verified commit for {repo}: {reason}; \
         batchChanges.rejectUnverifiedCommit is set, so the commit is not pushed with git instead"
    )]
    UnverifiedCommitRejected { repo: String, reason: String },
    /// The code host rejected the push credential.
    #[error("pushing to {repo}: the code host rejected the credential: {message}")]
    PushUnauthenticated { repo: String, message: String },
    /// The changeset spec lacks what the operation needs.
    #[error("invalid changeset spec: {0}")]
    InvalidSpec(String),
    /// The changeset or its repository is gone or inconsistent.
    #[error("{0}")]
    Invalid(String),
    /// A code host call failed.
    #[error("{context}: {source}")]
    CodeHost {
        context: &'static str,
        #[source]
        source: CodeHostError,
    },
    /// A repo node call failed.
    #[error("{context}: {source}")]
    Git {
        context: &'static str,
        #[source]
        source: GitCallError,
    },
    /// The repository has no repo node to commit on (none configured).
    #[error("no repository nodes are configured, so changeset commits cannot be created")]
    NoRepoNodes,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl ReconcileError {
    /// A code host error with context.
    pub fn code_host(context: &'static str, source: CodeHostError) -> Self {
        Self::CodeHost { context, source }
    }

    /// Whether retrying later may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::MissingCredentials { .. }
            | Self::SameBranch
            | Self::ReadOnly(_)
            | Self::PatchFailed { .. }
            | Self::UnverifiedCommitRejected { .. }
            | Self::PushUnauthenticated { .. }
            | Self::InvalidSpec(_)
            | Self::Invalid(_) => false,
            Self::CodeHost { source, .. } => source.is_temporary(),
            Self::Git { source, .. } => matches!(
                source,
                GitCallError::Unavailable(_)
                    | GitCallError::Internal(_)
                    | GitCallError::RepoNotCloned
                    | GitCallError::CloneInProgress
                    | GitCallError::NonFastForward(_)
                    | GitCallError::RevisionNotFound(_)
            ),
            Self::NoRepoNodes => true,
            // A missing keyring or undecryptable credential needs an admin.
            Self::Db(DbError::Invalid(_) | DbError::Crypto(_)) => false,
            Self::Db(_) => true,
        }
    }
}

impl From<ReconcileError> for JobError {
    fn from(e: ReconcileError) -> Self {
        if e.is_retryable() {
            JobError::retry(&e)
        } else {
            JobError::fatal(&e)
        }
    }
}

/// Result of reconciler calls.
pub type Result<T, E = ReconcileError> = std::result::Result<T, E>;
