//! sourcegull-search-exec: node-side search executors.
//!
//! [`Matcher`](sourcegull_search::Matcher) evaluates a job's pattern tree against
//! file contents; [`chunk_matches`] groups the match ranges into the
//! line chunks the streaming API returns. [`search_commit`] runs an
//! unindexed search over one commit's tree; [`SearchNodeService`] serves it
//! over gRPC on repo nodes (`sourcegull-search-client` fans searches out to them
//! from `web`). [`ShardIndex`] is the indexed engine behind
//! [`sourcegull_search::index::SearchIndex`]: trigram queries from patterns
//! ([`trigram_query`]) narrow candidates, which the same matcher verifies;
//! small repositories share compound shards. Both backends also run symbol
//! search and structural search (`sourcegull-structural`), and score matches for
//! ranking.
//! [`OwnerCheck`] filters and reports file owners on the nodes (Code Ownership).
//! [`series`] counts trend series at points of a repository's
//! history, incrementally through tree diffs.

mod commits;
mod compound;
mod diff_preview;
mod indexed;
pub mod langstats;
mod owners;
mod pool;
pub mod ranking;
pub mod series;
mod service;
mod symbol_cache;
mod trigrams;
mod unindexed;

pub use commits::{
    CommitFilters, CommitMatch, CommitSearchEnd, CommitSearchOptions, DiffCache, DiffFileMatch,
    DiffHunkMatch, search_commits,
};
pub use compound::MergeReport;
pub use indexed::{
    CompiledQuery, CompoundOptions, IndexUpdate, RepoShards, ShardIndex, ShardMatch,
    branches_to_index, search_shard, update_index,
};
pub use owners::{ContributorCheck, OwnerCheck, OwnerSearchExt, find_codeowners};
pub use service::{AlwaysIndexFn, IndexControl, SearchNodeService};
pub use sourcegull_gitstore::read::DEFAULT_MAX_FILE_SIZE;
pub use sourcegull_search::chunks::{ChunkMatch, Location, MatchRange, SymbolMatch, chunk_matches};
pub use sourcegull_search::index;
pub use sourcegull_search::{CompileError, PatternSpec};
pub use trigrams::{TrigramQuery, extract as trigram_query};
pub use unindexed::{
    FileMatch, PathFilter, SearchStats, UnindexedOptions, search_commit, search_commit_symbols,
};
