//! Error type for git storage operations.

use std::time::Duration;

use sourcegull_rpc::ErrorKind;
use sourcegull_types::{RemoteUnavailable, RepoId};

use crate::sync::RemoteHint;

/// Errors returned by gitstore operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The repo is not cloned on this node.
    #[error("repo {0} is not cloned")]
    RepoNotFound(RepoId),
    /// Another task is cloning the repo right now.
    #[error("repo {0} is being cloned")]
    CloneInProgress(RepoId),
    /// The revision does not resolve to a commit.
    #[error("revision not found: {0}")]
    RevisionNotFound(String),
    /// The path does not exist in the tree.
    #[error("path not found: {0}")]
    PathNotFound(String),
    /// The path exists but is not of the expected type.
    #[error("{path}: expected a {expected}")]
    WrongEntryType {
        /// The path that was looked up.
        path: String,
        /// What the operation needed (`file`, `directory`).
        expected: &'static str,
    },
    /// A size or count limit was exceeded.
    #[error("{what} exceeds the limit of {limit}")]
    TooLarge {
        /// What was too large.
        what: String,
        /// The limit.
        limit: u64,
    },
    /// The caller passed an invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// A git subprocess exited unsuccessfully. `stderr` has credentials redacted.
    #[error("git {op} failed ({status}): {stderr}")]
    Command {
        /// The git subcommand (`fetch`, `clone`, …).
        op: String,
        /// Exit status description.
        status: String,
        /// Tail of stderr, redacted.
        stderr: String,
    },
    /// A git subprocess or operation exceeded its timeout.
    #[error("git {op} timed out after {after:?}")]
    Timeout {
        /// The operation.
        op: String,
        /// The timeout that elapsed.
        after: Duration,
    },
    /// The operation was cancelled by the caller.
    #[error("git {0} cancelled")]
    Cancelled(String),
    /// The repository is corrupt (fsck failed or objects are missing).
    #[error("repository is corrupt: {0}")]
    Corrupt(String),
    /// Filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Error from gitoxide.
    #[error("git: {0}")]
    Gix(String),
    /// A patch does not apply to its base commit. `stderr` is `git apply`'s output.
    #[error("patch does not apply: {stderr}")]
    PatchFailed {
        /// Tail of `git apply`'s stderr.
        stderr: String,
    },
    /// A write (clone, fetch, commit, push) of a repo assigned to another node.
    #[error("repository {0} is assigned to another node")]
    NotAssigned(RepoId),
    /// The remote rejected a push. `stderr` has credentials redacted.
    #[error("push rejected ({reason}): {stderr}")]
    PushRejected {
        /// Why, as far as git's output tells.
        reason: PushRejection,
        /// Tail of `git push`'s stderr, redacted.
        stderr: String,
    },
}

/// Why a remote rejected a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushRejection {
    /// Missing, invalid or expired credentials.
    Unauthenticated,
    /// No write access, a protected branch, an archived or a missing project.
    PermissionDenied,
    /// Not a fast-forward and not forced.
    NonFastForward,
}

impl std::fmt::Display for PushRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PushRejection::Unauthenticated => "authentication failed",
            PushRejection::PermissionDenied => "permission denied",
            PushRejection::NonFastForward => "non-fast-forward",
        })
    }
}

impl GitError {
    /// Whether a failed clone or fetch means the code host has no repository the node can
    /// fetch, and why; `None` for a real failure. GitLab answers "A repository for this
    /// project does not exist yet" for a project whose repository was never created (API
    /// field `empty_repo`), and "You are not allowed to download code from this project"
    /// both when the project's repository feature is disabled and when the token is below
    /// Reporter on a private project; `hint` (what the code host's listing said about the
    /// project) tells those two apart and lets a plain "repository not found" count too.
    /// Without a hint, "not allowed to download code" is [`RemoteUnavailable::NoCodeAccess`].
    pub fn remote_unavailable(&self, hint: &RemoteHint) -> Option<RemoteUnavailable> {
        let GitError::Command { op, stderr, .. } = self else {
            return None;
        };
        if op != "fetch" && op != "ls-remote" {
            return None;
        }
        let msg = stderr.to_ascii_lowercase();
        let denied_or_disabled = || {
            if hint.repository_disabled {
                RemoteUnavailable::Disabled
            } else {
                RemoteUnavailable::NoCodeAccess
            }
        };
        if msg.contains("a repository for this project does not exist yet") {
            Some(RemoteUnavailable::Missing)
        } else if msg.contains("not allowed to download code") {
            Some(denied_or_disabled())
        } else if msg.contains("repository") && msg.contains("not found") {
            if hint.empty {
                Some(RemoteUnavailable::Missing)
            } else if hint.repository_disabled || hint.below_reporter {
                Some(denied_or_disabled())
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Wraps any gitoxide error.
    pub(crate) fn gix(e: impl std::fmt::Display) -> Self {
        GitError::Gix(e.to_string())
    }

    /// The RPC error kind for this error.
    pub fn kind(&self) -> ErrorKind {
        match self {
            GitError::RepoNotFound(_) => ErrorKind::RepoNotFound,
            GitError::CloneInProgress(_) => ErrorKind::CloneInProgress,
            GitError::RevisionNotFound(_) => ErrorKind::RevisionNotFound,
            GitError::PathNotFound(_) => ErrorKind::PathNotFound,
            GitError::WrongEntryType { .. } => ErrorKind::WrongEntryType,
            GitError::TooLarge { .. } => ErrorKind::TooLarge,
            GitError::InvalidArgument(_) => ErrorKind::InvalidArgument,
            GitError::Command { .. } | GitError::Cancelled(_) => ErrorKind::CommandFailed,
            GitError::Timeout { .. } => ErrorKind::Timeout,
            GitError::Corrupt(_) | GitError::Io(_) | GitError::Gix(_) => ErrorKind::Unavailable,
            GitError::PatchFailed { .. } => ErrorKind::PatchFailed,
            GitError::NotAssigned(_) => ErrorKind::NotAssigned,
            GitError::PushRejected { reason, .. } => match reason {
                PushRejection::Unauthenticated => ErrorKind::Unauthenticated,
                PushRejection::PermissionDenied => ErrorKind::PermissionDenied,
                PushRejection::NonFastForward => ErrorKind::NonFastForward,
            },
        }
    }
}

impl From<gix::Error> for GitError {
    fn from(e: gix::Error) -> Self {
        GitError::gix(e)
    }
}

impl From<GitError> for tonic::Status {
    fn from(e: GitError) -> Self {
        e.kind().status(e.to_string())
    }
}

/// Result alias for gitstore.
pub type Result<T, E = GitError> = std::result::Result<T, E>;
