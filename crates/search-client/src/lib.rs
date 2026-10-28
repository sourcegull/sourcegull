//! sourcegull-search-client: the `web` side of the repo nodes' search service.
//!
//! [`search_repos`] fans a content, path, symbol, commit or diff search ([`ContentSearch`])
//! out to the repo nodes that own the repositories, indexed first and unindexed for the
//! revisions the index does not hold, and streams the results back as search events.
//! [`repo_predicates`] evaluates `rev:at.time()`, `repo:has.commit.after()` and
//! `repo:has.file()` over resolved repositories on the nodes. [`OwnerSearch`] is the owner
//! part of a search (Code Ownership) as it is sent to the nodes, and the `*_to_pb` /
//! `*_from_pb` functions convert patterns, file filters and commit filters to and from
//! their wire forms; the node side (`sourcegull-search-exec`) reads them back.

mod fanout;
mod owners;
pub mod repo_predicates;
mod wire;

pub use fanout::{
    ContentSearch, diff_lines, owner_matches, search_repos, to_commit_match, to_match,
};
pub use owners::{ContributorTerm, OwnerSearch, RepoOwnerData};
pub use wire::{
    CommitFilterSpec, commit_filters_from_pb, commit_filters_to_pb, path_filter_from_pb,
    path_filter_to_pb, spec_from_pb, spec_to_pb,
};
