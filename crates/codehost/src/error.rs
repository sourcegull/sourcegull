//! Errors returned by code host clients.

use std::time::Duration;

/// Result alias for this crate.
pub type Result<T, E = CodeHostError> = std::result::Result<T, E>;

/// Errors from talking to a code host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodeHostError {
    /// The credentials were rejected (HTTP 401).
    #[error("unauthorized: the code host rejected the token")]
    Unauthorized,
    /// The token lacks access (HTTP 403).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The object does not exist or is not visible to the token (HTTP 404).
    #[error("not found: {0}")]
    NotFound(String),
    /// The code host kept rate limiting us beyond the retry budget.
    #[error("rate limited by the code host (retry after {retry_after:?})")]
    RateLimited {
        /// How long the code host asked us to wait, if it said.
        retry_after: Option<Duration>,
    },
    /// Any other unexpected HTTP status.
    #[error("unexpected HTTP status {status}: {message}")]
    Http {
        /// The status code.
        status: u16,
        /// The (truncated) response body or reason.
        message: String,
    },
    /// The request could not be sent or the response not received.
    #[error("request failed: {0}")]
    Transport(String),
    /// The response could not be decoded.
    #[error("invalid response: {0}")]
    Decode(String),
    /// The connection configuration is unusable.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The repository is archived (read-only): changesets can no longer be pushed or
    /// changed (external state `READONLY`).
    #[error("repository is archived: {0}")]
    Archived(String),
    /// The code host refused to merge the changeset (conflicts, failing checks, missing
    /// approvals, …).
    #[error("changeset cannot be merged: {0}")]
    NotMergeable(String),
    /// The user's token was granted without a scope the operation needs; signing in again
    /// grants it.
    #[error(
        "the token lacks the {0:?} scope: sign in again to grant access to private repositories"
    )]
    MissingScope(String),
    /// The operation is not supported by this code host or its version.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl CodeHostError {
    /// Whether retrying later may succeed (rate limits, transport errors, 5xx).
    pub fn is_temporary(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transport(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }
}
