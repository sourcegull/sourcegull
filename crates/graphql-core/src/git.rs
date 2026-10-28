//! Mapping repo node call errors to GraphQL errors, and the file size limit of reads.

use sourcegull_services::git::GitCallError;

use crate::context::{codes, error};

/// Bytes of a file returned by `GitBlob.content`; larger files are cut (`contentTruncated`).
pub const BLOB_CONTENT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Maps a repo node error to a GraphQL error with a stable code.
pub fn git_error(e: GitCallError) -> async_graphql::Error {
    let code = match &e {
        GitCallError::RepoNotCloned => codes::REPO_NOT_CLONED,
        GitCallError::CloneInProgress => codes::CLONE_IN_PROGRESS,
        GitCallError::RevisionNotFound(_) => codes::REVISION_NOT_FOUND,
        GitCallError::PathNotFound(_) => codes::NOT_FOUND,
        GitCallError::WrongEntryType(_) | GitCallError::InvalidArgument(_) => codes::INVALID_INPUT,
        GitCallError::TooLarge(_) => codes::TOO_LARGE,
        GitCallError::PatchFailed(_) => codes::INVALID_INPUT,
        GitCallError::Unauthenticated(_)
        | GitCallError::PermissionDenied(_)
        | GitCallError::NonFastForward(_) => codes::FORBIDDEN,
        GitCallError::Unavailable(m) => {
            tracing::warn!(error = %m, "graphql: repo node unavailable");
            codes::UNAVAILABLE
        }
        GitCallError::Internal(m) => {
            tracing::error!(error = %m, "graphql: repo node call failed");
            codes::INTERNAL
        }
    };
    error(code, e.to_string())
}
