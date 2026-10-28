//! sourcegull-gitstore: bare repository management for the repo node.
//!
//! - [`GitStore`]: the per-node manager. Layout ([`Layout`]), clone/fetch with
//!   per-repo locking and bounded concurrency, status, janitor, read access.
//! - Network and maintenance use the `git` CLI ([`command`], [`sync`],
//!   [`janitor`]); credentials go through `GIT_ASKPASS` and never appear in
//!   argv, URLs on disk or logs ([`remote`]).
//! - Bulk changes writes: commits from patches in a temporary index
//!   ([`patch`]) and pushes with per-request credentials ([`push`]).
//! - Reads use `gix` in-process ([`read`], [`diff`], [`graph`], [`refs`]);
//!   blame and archives stream from the CLI ([`blame`], [`archive`]).
//! - [`copy`]: copying a repo to another repo node when nodes are added or removed
//!   (rebalancing): a ref snapshot plus a pack, verified on arrival.
//! - [`schedule`]: the fetch scheduler policy (a pure function) and priority queue.
//! - [`service`]: the `sourcegull.repo.v1.RepoService` gRPC server over a
//!   [`GitStore`], generic over a [`RemoteResolver`]. No database access.

pub mod archive;
pub mod blame;
pub mod changes;
pub mod command;
pub mod commitlog;
pub mod copy;
pub mod diff;
mod error;
pub mod graph;
pub mod janitor;
mod layout;
pub mod parallel;
pub mod patch;
pub mod push;
pub mod read;
pub mod refs;
pub mod remote;
pub mod schedule;
pub mod service;
mod store;
pub mod sync;
pub mod upload_pack;

pub use copy::{CopyHeader, CopySource};
pub use error::{GitError, PushRejection, Result};
pub use gix::ObjectId;
pub use layout::Layout;
pub use remote::{Remote, RemoteAuth, Secret};
pub use service::{RemoteResolver, RepoNodeService, RepoRef, ServiceConfig};
pub use store::{GitStore, GitStoreConfig, RepoStatus, SyncHooks};
pub use sync::{RemoteHint, SyncOutcome};
