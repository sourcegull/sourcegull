//! Outbound webhooks.
//!
//! Site admins register webhooks (a URL, an HMAC secret and the event types they want; the
//! GraphQL API is in `sourcegull-graphql`, the tables in [`store`]). The
//! bulk changes code emits events, and the worker role delivers them. Watch actions that post
//! to a URL are queued on the same queue as channel jobs and delivered by the same sender:
//!
//! - [`channel`]: [`Channel`]s (generic webhook, Slack, Mattermost, Telegram) and
//!   [`Sender::deliver_channel`], which sends one rendered message through the guard and logs
//!   the attempt.
//! - [`events`]: the event type keys (`batch_change:apply`, `changeset:publish`, …).
//! - [`payload`]: the JSON payloads of bulk change and changeset events.
//! - [`enqueue`]: [`Events`] and [`enqueue_event`], which queue an event when some webhook
//!   subscribes to it.
//! - [`sender`]: the pgqueue worker that signs and sends events and sends channel jobs, with
//!   retries and a log per attempt.
//! - [`guard`]: the SSRF guard every delivery goes through (non-public addresses refused
//!   unless allowed by `webhooks.outbound.allowedHosts`).
//! - [`store`]: the webhooks, their event types, the queue of events and channel jobs, and the
//!   delivery logs (encrypted with the `outbound_webhook` data key).
//! - [`janitor`]: deletes logs (of webhooks and channels) older than `webhooks.outbound.logRetention`.

pub mod channel;
pub mod enqueue;
pub mod events;
mod feature;
pub mod guard;
pub mod janitor;
pub mod payload;
pub mod sender;
pub mod store;

pub use channel::{Channel, ChannelJobRef, ChannelMessage, TELEGRAM_API};
pub use enqueue::{Events, enqueue_event};
pub use events::{
    BATCH_CHANGE_APPLY, BATCH_CHANGE_CLOSE, BATCH_CHANGE_DELETE, CHANGESET_CLOSE,
    CHANGESET_PUBLISH, CHANGESET_PUBLISH_ERROR, CHANGESET_UPDATE, CHANGESET_UPDATE_ERROR,
    EVENT_TYPES, EventType,
};
pub use feature::OutboundWebhooksFeature;
pub use guard::{Allowlist, GuardError};
pub use janitor::{JANITOR, run_janitor};
pub use sender::{
    DELIVERY_HEADER, EVENT_TYPE_HEADER, SIGNATURE_HEADER, SendHandler, Sender, sender_workers, sign,
};
