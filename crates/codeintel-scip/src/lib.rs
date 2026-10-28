//! Precise code intelligence: processing SCIP uploads.
//!
//! The `worker` role dequeues uploads from `codeintel_uploads` ([`worker`]), streams each
//! index document by document ([`stream`]) and writes an immutable code graph file
//! ([`graph`]) to the blob store. Cross-upload lookups go through Postgres:
//! `codeintel_symbol_postings` (which uploads hold a global symbol) and
//! `codeintel_package_monikers` (which packages an upload defines or uses).

pub mod cache;
mod feature;
pub mod graph;
pub mod janitor;
pub mod process;
pub mod proto;
pub mod repo_commits;
pub mod store;
pub mod stream;
pub mod symbol;
pub mod visibility;
pub mod worker;

#[cfg(test)]
mod tests;

pub use cache::{CacheError, GraphCache};
pub use feature::UploadsFeature;
pub use graph::{CodeGraph, GraphSummary, MonikerKind};
pub use process::{ProcessError, Processed, process_index};
