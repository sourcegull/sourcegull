//! The bulk changes reconciler.
//!
//! - [`plan`]: the planner, a pure function from a changeset and its current and previous
//!   changeset specs to the [`Operation`]s that reconcile it.
//! - [`executor`]: [`Reconciler`], which plans a changeset and carries the operations out on
//!   the code host ([`sourcegull_codehost::ChangesetSource`]) and the repo nodes
//!   (`CreateCommitFromPatch` + push), with the user-then-site credential, and stores what
//!   it learned.
//! - [`rewire`]: the rewirer, which maps a new bulk change spec's changeset specs to changesets
//!   for previews and applies.
//! - [`source`]: [`SourceProvider`], the changeset source of a repository's code host
//!   connection ([`DbSourceProvider`] in production).
//! - [`worker`]: the pgqueue worker over the `changesets` queue run by the worker role.
//! - [`syncer`]: the periodic syncer with adaptive intervals (2 min to 8 h; 1 h to 24 h when
//!   the connection has webhooks).
//! - [`bulk`]: the pgqueue worker over `changeset_jobs` carrying out bulk operations
//!   (comment, merge, close, publish, detach, re-enqueue).
//! - [`retention`]: the janitor deleting changesets past `batchChanges.changesetsRetention`.
//! - [`previews`]: the janitor deleting bulk change specs and changeset specs nobody applied.
//! - [`webhooks`]: [`ChangesetWebhooks`], which applies `merge_request` and `pipeline`
//!   webhooks to changesets right away.
//! - [`scheduler`]: the rollout window [`Scheduler`], releasing scheduled changesets at the
//!   windows' rates.

pub mod bulk;
pub mod error;
pub mod executor;
mod feature;
pub mod plan;
pub mod previews;
pub mod retention;
pub mod rewire;
pub mod scheduler;
pub mod source;
pub mod syncer;
pub mod webhooks;
pub mod worker;

pub use bulk::{BulkHandler, bulk_workers, process_job};
pub use error::{ReconcileError, Result};
pub use executor::{DEFAULT_SYNC_DELAY, FORK_USER_NAMESPACE, Reconciler, webhook_events};
pub use feature::ReconcilerFeature;
pub use plan::{Delta, DesiredPublication, Operation, Plan, plan, plan_with_current};
pub use previews::{
    PREVIEWS_JANITOR, PreviewsDeleted, delete_expired_previews, run_previews_janitor,
};
pub use retention::{RETENTION_JANITOR, purge_expired, run_retention_janitor};
pub use rewire::{Mapping, NEW_BATCH_CHANGE, Rewire, mappings, preview};
pub use scheduler::{
    Clock, FakeClock, SCHEDULER, Scheduler, SystemClock, Tick, TokenBucket, run_scheduler,
};
pub use source::{DbSourceProvider, SourceProvider};
pub use syncer::{SYNCER, next_sync, run_syncer, sync_due};
pub use webhooks::ChangesetWebhooks;
pub use worker::{ReconcileHandler, reconciler_workers};
