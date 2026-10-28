//! The event types webhooks subscribe to, with their keys.

/// A bulk change was created or applied.
pub const BATCH_CHANGE_APPLY: &str = "batch_change:apply";
/// A bulk change was closed.
pub const BATCH_CHANGE_CLOSE: &str = "batch_change:close";
/// A bulk change was deleted.
pub const BATCH_CHANGE_DELETE: &str = "batch_change:delete";
/// A changeset was published on the code host (opened, possibly as a draft).
pub const CHANGESET_PUBLISH: &str = "changeset:publish";
/// Publishing a changeset failed.
pub const CHANGESET_PUBLISH_ERROR: &str = "changeset:publish_error";
/// A published changeset was updated (new commits, title or body, undrafted, reopened).
pub const CHANGESET_UPDATE: &str = "changeset:update";
/// Updating a published changeset failed.
pub const CHANGESET_UPDATE_ERROR: &str = "changeset:update_error";
/// A changeset was closed on the code host.
pub const CHANGESET_CLOSE: &str = "changeset:close";

/// An event type with its description (GraphQL `OutboundWebhookEventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventType {
    pub key: &'static str,
    pub description: &'static str,
}

/// Every event type, in the order the API lists them.
pub const EVENT_TYPES: &[EventType] = &[
    EventType {
        key: BATCH_CHANGE_APPLY,
        description: "sent when a bulk change is applied or created",
    },
    EventType {
        key: BATCH_CHANGE_CLOSE,
        description: "sent when a bulk change is closed",
    },
    EventType {
        key: BATCH_CHANGE_DELETE,
        description: "sent when a bulk change is deleted",
    },
    EventType {
        key: CHANGESET_CLOSE,
        description: "sent when a changeset is closed",
    },
    EventType {
        key: CHANGESET_PUBLISH,
        description: "sent when a changeset is published to the code host",
    },
    EventType {
        key: CHANGESET_PUBLISH_ERROR,
        description: "sent when an error occurs publishing a changeset to the code host",
    },
    EventType {
        key: CHANGESET_UPDATE,
        description: "sent when a changeset is updated on the code host",
    },
    EventType {
        key: CHANGESET_UPDATE_ERROR,
        description: "sent when an error occurs updating a changeset on the code host",
    },
];

/// Whether `key` is a known event type.
pub fn is_known(key: &str) -> bool {
    EVENT_TYPES.iter().any(|t| t.key == key)
}
