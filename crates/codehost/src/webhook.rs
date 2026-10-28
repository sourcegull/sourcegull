//! Incoming webhooks: [`WebhookHandler`] turns a code host's request into host-agnostic
//! [`DomainEvent`]s.

use chrono::{DateTime, Utc};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::changeset::{ChangesetEvent, ChangesetState, CheckState, ReviewState};

/// Key prefix of changeset events made from webhooks. The code host's own ids (note and state
/// event ids) are not in webhook payloads, so a full sync, which has them, replaces the
/// events with this prefix.
pub const HOOK_EVENT_PREFIX: &str = "hook:";

/// Why a webhook request was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// The shared secret is missing or wrong (respond 401).
    #[error("invalid or missing webhook secret")]
    Unauthorized,
    /// The body or headers are malformed (respond 400).
    #[error("malformed webhook: {0}")]
    Malformed(String),
}

/// One ref change announced by a code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefChange {
    /// Full ref name, e.g. `refs/heads/main` or `refs/tags/v1.0`.
    pub name: String,
    /// `None` when the ref was created.
    pub old_oid: Option<String>,
    /// `None` when the ref was deleted.
    pub new_oid: Option<String>,
}

/// A change to a repository's existence or location (system-level events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoChange {
    Created,
    Destroyed,
    Renamed,
    Transferred,
}

/// What a webhook means, independent of the code host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomainEvent {
    /// Refs of a repo moved: fetch it now.
    RefsUpdated {
        /// The repo's external id on the code host.
        external_id: String,
        /// The changes, as claimed by the code host.
        refs: Vec<RefChange>,
    },
    /// A repo was created, destroyed, renamed or moved: re-list the connection.
    RepoChanged {
        /// The repo's external id, when the event carries one.
        external_id: Option<String>,
        /// What happened.
        change: RepoChange,
    },
    /// A merge request changed: update the changeset (bulk changes).
    ChangesetChanged {
        /// The repo's external id.
        external_id: String,
        /// The merge request number within the repo (GitLab `iid`).
        number: i64,
        /// What changed.
        hook: ChangesetHook,
    },
    /// A pipeline changed: refresh the CI state of its changeset.
    ChangesetChecksChanged {
        /// The repo's external id.
        external_id: String,
        /// The merge request number, when the pipeline belongs to one.
        number: Option<i64>,
        /// The pipeline's branch (full ref), for pipelines without a merge request.
        branch: Option<String>,
        /// The pipeline, when the payload describes one.
        checks: Option<ChecksHook>,
    },
    /// A user's access changed (group/project membership): sync that user's permissions.
    UserPermsChanged {
        /// The user's account id on the code host.
        account_id: String,
        /// The user's username on the code host, when the event carries it.
        username: Option<String>,
    },
    /// Who can read a repository changed (team access, visibility, transfer): sync that
    /// repository's permissions.
    RepoPermsChanged {
        /// The repo's external id on the code host.
        external_id: String,
    },
    /// A recognized event this system does not act on.
    Ignored {
        /// The event name, for the webhook log.
        event: String,
    },
}

/// What a merge request webhook says about a changeset. `None` fields were not in the
/// payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesetHook {
    /// The host's action, e.g. `open`, `close`, `approved` (for logs).
    pub action: Option<String>,
    /// The state after the change.
    pub state: Option<ChangesetState>,
    /// Set when the change decides the review state (approved, or no longer).
    pub review_state: Option<ReviewState>,
    /// The title as on the host (including any draft marker).
    pub title: Option<String>,
    pub body: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    /// Timeline events, keyed with [`HOOK_EVENT_PREFIX`].
    pub events: Vec<ChangesetEvent>,
    /// Changed parts of the changeset's host metadata (merged into the stored metadata, one
    /// level deep).
    pub metadata: Map<String, Value>,
    /// The payload does not carry everything that changed (e.g. new commits): sync the
    /// changeset from the code host soon.
    pub resync: bool,
}

/// What a pipeline webhook says about a changeset's checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksHook {
    /// The pipeline's id; newer pipelines have higher ids.
    pub pipeline_id: i64,
    /// The commit it ran on.
    pub sha: Option<String>,
    pub state: CheckState,
    /// The timeline event (keyed like a sync's, so a sync updates the same row).
    pub event: ChangesetEvent,
    /// Changed parts of the changeset's host metadata (the head pipeline).
    pub metadata: Map<String, Value>,
}

/// Verifies and parses one code host's webhooks.
pub trait WebhookHandler: Send + Sync {
    /// Checks the request's shared secret (in constant time).
    fn verify(&self, headers: &HeaderMap, body: &[u8]) -> Result<(), WebhookError>;

    /// Parses a verified request into events.
    fn parse(&self, headers: &HeaderMap, body: &[u8]) -> Result<Vec<DomainEvent>, WebhookError>;
}

/// Where a code host should send a connection's webhooks.
#[derive(Clone, PartialEq, Eq)]
pub struct HookTarget {
    /// The receiver URL. A repository's hooks whose URL has the same path (any scheme, host or
    /// query) are this connection's.
    pub url: String,
    /// The shared secret the code host sends with each delivery.
    pub secret: String,
}

impl std::fmt::Debug for HookTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookTarget")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// What [`WebhookRegistrar::ensure`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookChange {
    /// The repository had no hook to the receiver; one was created.
    Created,
    /// A hook existed but its URL or events differed; it was updated.
    Updated,
    /// The hook was already as wanted.
    Unchanged,
}

/// Registers a connection's webhooks on its repositories through the code host's API.
#[async_trait::async_trait]
pub trait WebhookRegistrar: Send + Sync {
    /// Makes repository `external_id` have exactly one hook to `target` with the events the
    /// receiver handles. Extra hooks to the receiver are deleted.
    async fn ensure(&self, external_id: &str, target: &HookTarget) -> crate::Result<HookChange>;

    /// Deletes repository `external_id`'s hooks to `target`; returns how many were deleted.
    async fn remove(&self, external_id: &str, target: &HookTarget) -> crate::Result<usize>;
}
