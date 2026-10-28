//! Code Ownership.
//!
//! - [`access`] (feature `store`): who may create and change teams.
//! - [`codeowners`]: GitLab CODEOWNERS files: the parser (sections, optional sections,
//!   approval counts, default owners, `@user`, `@group/subgroup`, emails, roles) and the
//!   `globset` matcher with GitLab's pattern semantics, cached by blob OID.
//! - [`filter`]: the owner filter of `file:has.owner()` and the owners `select:file.owners`
//!   reports, evaluated on repo nodes on candidate paths.
//! - [`store`] (feature `store`): teams and members, ingested CODEOWNERS files and assigned
//!   owners and teams.
//! - [`resolve`] (feature `store`): handles and emails to users and teams.
//! - [`ownership`] (feature `store`): the ranked owner list of a file or directory with its
//!   reasons: CODEOWNERS entries, assignments and the signals.
//! - [`signals`] (feature `store`): the recent contributors and recent viewers signals, their
//!   configuration, and ownership analytics (coverage per directory).
//! - [`teamsync`] (feature `teamsync`): teams synced from GitLab groups
//!   (`own.syncTeamsFromGitLabGroups`), and (feature `jobs`) its background loop.
//! - [`jobs`] (feature `jobs`): the background indexers of the signals and analytics, and
//!   their scheduler.
//!
//! CODEOWNERS files use GitLab's syntax. The GraphQL API is in `sourcegull-graphql`.

#[cfg(feature = "store")]
pub mod access;
pub mod codeowners;
#[cfg(feature = "jobs")]
mod feature;
pub mod filter;
#[cfg(feature = "jobs")]
pub mod jobs;
#[cfg(feature = "store")]
pub mod ownership;
#[cfg(feature = "store")]
pub mod resolve;
#[cfg(feature = "store")]
pub mod signals;
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "teamsync")]
pub mod teamsync;

pub use codeowners::{
    CodeownersCache, CompiledCodeowners, OwnerRef, codeowners_paths, owner_key, text_key,
};
#[cfg(feature = "jobs")]
pub use feature::OwnFeature;
pub use filter::{AssignedEntry, OwnerTerm, RepoOwners};

/// The process-wide cache of compiled CODEOWNERS files (keyed by blob OID or text digest).
pub fn codeowners_cache() -> &'static CodeownersCache {
    static CACHE: std::sync::OnceLock<CodeownersCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(CodeownersCache::default)
}
