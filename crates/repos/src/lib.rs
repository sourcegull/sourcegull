//! Host-agnostic repo plumbing: everything about code host repositories except git storage.
//!
//! - [`Registry`]: maps a connection kind to its code host implementations.
//! - [`Syncer`]: discovery sync of one connection: list, upsert, delete unlisted, queue
//!   clones and fetches, record the outcome.
//! - [`worker`]: the leader-elected sync scheduler and the sync queue handler.
//! - [`oauth_tokens`]: GitLab OAuth connection tokens, refreshed and stored back.
//! - [`WebhookIngest`]: routes, verifies, parses and applies webhook deliveries.
//!
//! The repo node consumes what this crate produces: `repo_update_jobs` rows (clone/fetch
//! requests with a priority) and `repo_ref_events` rows; see `sourcegull_db::RepoUpdateJobStore`.

mod feature;
pub mod oauth_tokens;
pub mod outbound_log;
mod registry;
pub mod sync;
pub mod webhooks;
pub mod worker;
pub use feature::ReposFeature;

pub use registry::{ConfigCheck, Registry};
pub use sync::{SyncError, Syncer};
pub use webhooks::{ChangesetHooks, WebhookIngest, WebhookOutcome, WebhookTarget};
