//! Code intelligence configuration policies: which branches, tags and
//! commits of a repository policies name ([`matcher`]), how long precise indexes visible
//! from them are kept ([`retention`]), and the expirer that marks the rest
//! ([`expirer`]). Auto-indexing will match the same policies for indexing.
//!
//! Policies are stored by [`crate::store::PolicyStore`]; git data comes from the repository
//! nodes through [`git::RepoGit`].

pub mod expirer;
mod feature;
pub mod fields;
pub mod git;
pub mod matcher;
pub mod retention;
pub mod store;
pub use feature::RetentionFeature;

pub use git::RepoGit;
pub use matcher::{BranchMatch, PolicyMatches, Purpose, TipMatch, match_policies};
pub use retention::{PolicyOverview, Protection, Retention, overview};

/// Errors of policy matching and retention.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("git: {0}")]
    Git(#[from] sourcegull_services::git::GitCallError),
    #[error("database: {0}")]
    Db(sourcegull_db::DbError),
}
