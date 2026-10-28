//! Auto-indexing: which index jobs a repository needs at a commit, and the scheduler that queues
//! them for executors.
//!
//! - [`inference`]: sandboxed Lua recognizers infer jobs from the repository's files; the
//!   site's inference script can add or disable recognizers.
//! - [`config`]: index configuration (`sourcegull.yaml`, or JSON stored per repository).
//! - [`service`]: which jobs a commit gets (explicit, saved, in-repository or inferred
//!   configuration) and queueing them with deduplication.
//! - [`scheduler`]: the leader-elected `worker` loop that queues jobs on ref updates and in a
//!   periodic sweep, for commits that configuration policies with indexing enabled name.

pub mod config;
mod feature;
pub mod glob;
pub mod indexers;
pub mod inference;
pub mod scheduler;
pub mod service;
pub mod store;
pub use feature::AutoIndexFeature;

pub use config::{DockerStep, IndexConfiguration, IndexJobSpec};
pub use indexers::IndexerMap;
pub use inference::{InferenceError, InferenceResult, Limits, RepoFiles, infer};
pub use service::{AutoIndexError, AutoIndexer, JobSource, RepoRef};
