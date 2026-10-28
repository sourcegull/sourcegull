//! Shared domain types for Sourcegull: id newtypes and the core entities that
//! cross crate boundaries (users, emails, orgs, namespaces, code host connections
//! and repos).
//!
//! Ids are transparent newtypes: they serialize as plain numbers, print and
//! parse as plain numbers, and (with the default `sqlx` feature) map directly
//! to Postgres `bigint`/`int` columns. Repo ids are `i32` so they fit in roaring
//! bitmaps; every other id is `i64`.

pub mod always_index;
mod authz;
pub mod glob;
mod ids;
pub mod metric_names;
pub mod ranking;
mod repo;
mod symbol_kind;
mod user;

pub use always_index::{ALWAYS_INDEX_MAX_FILE_SIZE, AlwaysIndex};
pub use authz::RepoAccess;
pub use ids::*;
pub use ranking::PathRanks;
pub use repo::*;
pub use symbol_kind::SymbolKind;
pub use user::*;
