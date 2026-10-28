//! Typed error kinds carried in gRPC status metadata.
//!
//! A status code alone cannot tell "repo not cloned" from "revision not found"
//! (both are `NOT_FOUND`), and the web role needs that distinction to choose
//! between "cloning in progress" and a 404. Servers attach
//! [`ERROR_KIND_METADATA_KEY`]; clients read it back with [`ErrorKind::of`].

use std::fmt;
use std::str::FromStr;

use tonic::{Code, Status};

/// Metadata key holding the [`ErrorKind`] of a failed call.
pub const ERROR_KIND_METADATA_KEY: &str = "sourcegull-error-kind";

/// What went wrong, independent of the gRPC status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The repo is not cloned on this node.
    RepoNotFound,
    /// The repo is being cloned right now.
    CloneInProgress,
    /// The revision does not resolve to a commit.
    RevisionNotFound,
    /// The path does not exist at the commit.
    PathNotFound,
    /// The path exists but has the wrong type (e.g. a directory for ReadFile).
    WrongEntryType,
    /// A size or count limit was exceeded.
    TooLarge,
    /// The request was malformed.
    InvalidArgument,
    /// A git command or the remote failed (clone, fetch, push, maintenance).
    CommandFailed,
    /// The operation hit its deadline.
    Timeout,
    /// The node is out of disk or the repo is corrupt.
    Unavailable,
    /// A patch does not apply to its base commit (CreateCommitFromPatch).
    PatchFailed,
    /// The remote rejected the push credentials (missing, invalid, expired).
    Unauthenticated,
    /// The remote refused the push: no write access, a protected branch, an
    /// archived or missing project. Bulk changes treat this as read-only.
    PermissionDenied,
    /// The push was rejected because it is not a fast-forward.
    NonFastForward,
    /// A write (clone, fetch, commit, push) reached a node the repository is not assigned
    /// to, as when it moved to another node and the caller's routing table is behind.
    NotAssigned,
}

impl ErrorKind {
    /// Every kind (for tests and exhaustive mapping).
    pub const ALL: [ErrorKind; 15] = [
        ErrorKind::RepoNotFound,
        ErrorKind::CloneInProgress,
        ErrorKind::RevisionNotFound,
        ErrorKind::PathNotFound,
        ErrorKind::WrongEntryType,
        ErrorKind::TooLarge,
        ErrorKind::InvalidArgument,
        ErrorKind::CommandFailed,
        ErrorKind::Timeout,
        ErrorKind::Unavailable,
        ErrorKind::PatchFailed,
        ErrorKind::Unauthenticated,
        ErrorKind::PermissionDenied,
        ErrorKind::NonFastForward,
        ErrorKind::NotAssigned,
    ];

    /// Stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::RepoNotFound => "repo_not_found",
            ErrorKind::CloneInProgress => "clone_in_progress",
            ErrorKind::RevisionNotFound => "revision_not_found",
            ErrorKind::PathNotFound => "path_not_found",
            ErrorKind::WrongEntryType => "wrong_entry_type",
            ErrorKind::TooLarge => "too_large",
            ErrorKind::InvalidArgument => "invalid_argument",
            ErrorKind::CommandFailed => "command_failed",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::PatchFailed => "patch_failed",
            ErrorKind::Unauthenticated => "unauthenticated",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::NonFastForward => "non_fast_forward",
            ErrorKind::NotAssigned => "not_assigned",
        }
    }

    /// The gRPC code used for this kind.
    pub fn code(self) -> Code {
        match self {
            ErrorKind::RepoNotFound | ErrorKind::RevisionNotFound | ErrorKind::PathNotFound => {
                Code::NotFound
            }
            ErrorKind::CloneInProgress => Code::Unavailable,
            ErrorKind::WrongEntryType | ErrorKind::PatchFailed | ErrorKind::NotAssigned => {
                Code::FailedPrecondition
            }
            ErrorKind::TooLarge => Code::ResourceExhausted,
            ErrorKind::InvalidArgument => Code::InvalidArgument,
            ErrorKind::CommandFailed => Code::Internal,
            ErrorKind::Timeout => Code::DeadlineExceeded,
            ErrorKind::Unavailable => Code::Unavailable,
            ErrorKind::Unauthenticated => Code::Unauthenticated,
            ErrorKind::PermissionDenied => Code::PermissionDenied,
            ErrorKind::NonFastForward => Code::Aborted,
        }
    }

    /// Builds a status with this kind's code and metadata.
    pub fn status(self, message: impl Into<String>) -> Status {
        let mut status = Status::new(self.code(), message);
        status.metadata_mut().insert(
            ERROR_KIND_METADATA_KEY,
            tonic::metadata::MetadataValue::from_static(self.as_str()),
        );
        status
    }

    /// Reads the kind from a status, if the server set one.
    pub fn of(status: &Status) -> Option<ErrorKind> {
        status
            .metadata()
            .get(ERROR_KIND_METADATA_KEY)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown [`ErrorKind`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown error kind {0:?}")]
pub struct UnknownErrorKind(pub String);

impl FromStr for ErrorKind {
    type Err = UnknownErrorKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ErrorKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownErrorKind(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_through_status() {
        for kind in ErrorKind::ALL {
            let status = kind.status("boom");
            assert_eq!(status.code(), kind.code());
            assert_eq!(ErrorKind::of(&status), Some(kind));
            assert_eq!(kind.as_str().parse::<ErrorKind>(), Ok(kind));
        }
        assert_eq!(ErrorKind::of(&Status::internal("x")), None);
        assert!("nope".parse::<ErrorKind>().is_err());
    }
}
