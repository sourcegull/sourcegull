//! Code ownership as search reads it. The caller passes an [`Ownership`] in
//! ([`SearchBackend::ownership`](crate::SearchBackend::ownership)); `sourcegull-services`
//! implements it over the ownership store.

use std::collections::{HashMap, HashSet};

use sourcegull_db::DbError;
use sourcegull_search::stream::PersonUser;
use sourcegull_search_client::RepoOwnerData;

/// The ownership data `file:has.owner()` and `select:file.owners` need.
#[async_trait::async_trait]
pub trait Ownership: Send + Sync + std::fmt::Debug {
    /// The owner keys a `file:has.owner(term)` argument stands for.
    async fn term_keys(&self, term: &str) -> Result<HashSet<String>, DbError>;

    /// The ingested CODEOWNERS files, assigned owners and code host service types of
    /// `repos`, per repository id; GitLab repositories with neither file nor assignments have
    /// no entry.
    async fn repo_owner_data(&self, repos: &[i32]) -> Result<HashMap<i32, RepoOwnerData>, DbError>;

    /// Resolves owner references (`@handle` or an email) to users and teams, keyed by the
    /// reference as given. References that resolve to nothing are left out.
    async fn resolve(&self, refs: &[String]) -> Result<HashMap<String, ResolvedOwner>, DbError>;
}

/// What an owner reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOwner {
    /// One identity per user or team, so that the same owner reached through several
    /// references (a GitLab handle and an email) is sent once.
    pub identity: String,
    pub owner: Owner,
}

/// A resolved owner as search results show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    User(PersonUser),
    Team {
        name: String,
        display_name: String,
    },
    /// An unresolved group handle (`group/subgroup`, without `@`).
    ExternalTeam {
        handle: String,
    },
    /// An unresolved person.
    Person,
}
