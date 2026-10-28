//! Batch Changes workspace resolution.
//!
//! A server-side bulk change spec's workspaces are resolved by the `worker` role from the
//! `batch_spec_resolution_jobs` queue ([`worker`]), as the user who queued the job, so every
//! search and repository lookup applies their repository permissions. The [`Resolver`]:
//!
//! 1. Every `on` entry is resolved to repository revisions. `repositoriesMatchingQuery` runs
//!    the query through the search backend (`count:all` added unless the query has a
//!    `count:`, the default pattern type `standard` for spec version 1 and `keyword` for
//!    version 2); its matches reduce to their repositories (`select:repo` semantics), keeping
//!    the matched paths for `repository.search_result_paths`, on the default branch HEAD.
//!    Repositories not cloned yet are left out. `repository` entries are looked up by name,
//!    on their `branch`/`branches` or the default branch. Later entries replace earlier ones
//!    for the same repository, except that a query never replaces a `repository` entry.
//! 2. `workspaces[]`: a repository matching one entry's `in` glob (all repositories without
//!    `in`; matching two is an error) gets a workspace per directory holding a file named
//!    `rootAtLocationOf` (a `type:path` search at the resolved commit), and none if there is
//!    no such file; other repositories get one workspace at the root.
//! 3. Workspaces whose steps all have a statically false `if:` are dropped, as are all
//!    workspaces of a spec without steps. Each `if:` is partially evaluated per repository
//!    with what is known before any step runs (`repository.name`,
//!    `batch_change.name`/`description`, literals, `eq`/`ne`/`not` and the template functions
//!    over them); conditions that need more are left to run time. The execution cache lookup
//!    (6.) only considers the steps that may run in the workspace.
//! 4. Skips: archived repositories, repositories on a code host other than GitLab
//!    (`unsupported`, unless `allowUnsupported`) and repositories with a `.batchignore` file at
//!    the root (`ignored`, unless `allowIgnored`) are kept with `skipped` and a reason.
//! 5. `importChangesets` repositories are looked up (with permissions) and their external IDs
//!    validated; each ID becomes an `existing` changeset spec.
//! 6. Cache hits ([`cache`]): unless the spec has `noCache`, each workspace's step cache keys
//!    are looked up in `batch_spec_execution_cache_entries`; leading hits go into
//!    `step_cache_results` and a hit for the last step sets `cached_result_found`. Within a
//!    worker process, `.batchignore` checks and `rootAtLocationOf` searches are memoized per
//!    repository and commit, so a re-resolution only repeats them for repositories whose
//!    commit changed.
//!
//! The workspaces and import specs replace the spec's earlier ones in one transaction
//! ([`sourcegull_batches::store::BatchSpecWorkspaceStore::replace_resolution`]).

pub mod cache;
mod feature;
mod resolve;
pub mod worker;

pub use cache::{ResolutionCache, step_cache_key};
pub use feature::ResolverFeature;
pub use resolve::{
    ARCHIVED_REASON, IGNORED_REASON, Options, Resolution, ResolveError, Resolver,
    UNSUPPORTED_REASON, with_default_count,
};
