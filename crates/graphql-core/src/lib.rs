//! The foundation of the GraphQL API, shared by the schema assembly (`sourcegull-graphql`) and the
//! GraphQL domain crates (`sourcegull-<domain>-graphql`):
//!
//! - [`context`]: schema services, the viewer, error codes and error mapping;
//! - [`pagination`]: page info and opaque cursors;
//! - [`ids`]: node kinds and ID parsers of the shared types;
//! - [`events`]: the server's usage events;
//! - [`repo`]: the repository read rule;
//! - [`feature`]: a feature's part of the schema (root fields, node types and loaders);
//! - [`node`]: the `Node` interface and the `node(id:)` loader registry that domain crates
//!   contribute to;
//! - [`shared`]: late-bound references to the object types the assembly defines (`User`,
//!   `Org`, `Repository`), so that a domain crate can return them without depending on the
//!   assembly;
//! - [`declared`]: the read-only guards of objects owned by the configuration-as-code files;
//! - [`git`]: repo node call errors as GraphQL errors, and the file size limit;
//! - [`execution`]: the `ExecutionLogEntry` type of executor jobs;
//! - [`audit`]: security events;
//! - [`search`]: the `SearchPatternType` enum;
//! - [`diff`] and [`codehost`]: value types several domains return (`DiffStat`,
//!   `FileDiffHunk`, `ExternalServiceKind`, `ExternalLink`).

pub mod audit;
pub mod codehost;
pub mod context;
pub mod declared;
pub mod diff;
pub mod events;
pub mod execution;
pub mod feature;
pub mod git;
pub mod ids;
pub mod mask;
pub mod node;
pub mod pagination;
pub mod repo;
pub mod search;
pub mod shared;

pub use context::{Services, Viewer, codes};

/// The result of mutations that return nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyResponse;

#[async_graphql::Object]
impl EmptyResponse {
    /// Always null.
    async fn always_nil(&self) -> Option<String> {
        None
    }
}
