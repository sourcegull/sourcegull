//! Batch Changes.
//!
//! - [`spec`]: bulk change specs — types, YAML parsing with positions, schema and semantic validation,
//!   `published` resolution.
//! - [`changeset_spec`]: changeset specs — the per-changeset documents produced by executing a
//!   bulk change spec (or by `importChangesets`), with JSON (de)serialization and validation.
//! - [`cache_key`]: the cache key of step results.
//! - [`schema`]: the checked-in JSON Schemas.
//! - [`yaml`]: YAML to JSON with source positions; [`glob`]: repository name globs.
//! - `store` (feature `store`): the bulk change tables — specs, workspaces and their execution
//!   jobs, changeset specs, bulk changes, changesets and their events, changeset jobs and
//!   credentials.

pub mod cache_key;
pub mod changeset_spec;
pub mod error;
pub mod glob;
pub mod schema;
pub mod spec;
#[cfg(feature = "store")]
pub mod store;
pub mod yaml;

pub use changeset_spec::{
    BranchChangesetSpec, ChangesetSpec, ExistingChangesetSpec, GitCommitDescription,
    parse_changeset_spec,
};
pub use error::{Location, SpecErrors, SpecIssue};
pub use spec::{BatchSpec, ParsedBatchSpec, Published, PublishedValue, parse_batch_spec};
