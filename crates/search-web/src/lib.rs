//! sourcegull-search-web: search execution on the web side.
//!
//! [`run`] resolves a query's `context:` and the viewer's repository permissions, plans the
//! query and runs its jobs: content, path, symbol, commit and diff searches on the repo nodes
//! (through `sourcegull-search-client`) and repository searches on the repo catalog
//! ([`CatalogCache`]). The streaming search API streams its events; [`collect`],
//! [`spawn_search`] and [`resolve_repositories`] serve the other callers (GraphQL, MCP, code
//! monitors, insights, Search Jobs, bulk changes). Code ownership is read through
//! [`Ownership`], which the caller passes in with the [`SearchBackend`].

mod ownership;
mod search;

pub use ownership::{Owner, Ownership, ResolvedOwner};
pub use search::{
    CatalogCache, ResolvedContext, RunOptions, SearchBackend, SearchOutcome, StreamParams, collect,
    resolve_context, resolve_repositories, run, spawn_search,
};
