//! sourcegull-search: query planning.
//!
//! [`plan_query`] turns a query string into a [`Job`] tree: it parses and
//! validates the query (`sourcegull-query`), expands OR groups of filters into
//! [`BasicQuery`]s, resolves date expressions, and picks backend jobs per
//! result type. [`RepoMatcher`] resolves a job's repositories from an in-memory
//! [`RepoCatalog`], page by page, within a search context ([`ContextScope`]).
//! Executing the jobs is the search engine's part; [`stream`] has the streaming
//! API's event types. [`PatternSpec`] is a pattern lowered to regexps for the
//! executors, which [`Matcher`] compiles and evaluates against file contents
//! and names; [`index::SearchIndex`] is the interface to the indexed
//! engine. [`ranking`] scores files and repositories. [`exhaustive`] validates
//! and plans Search Jobs queries.

pub mod aggregate;
mod basic;
pub mod chunks;
mod context;
mod dates;
pub mod exhaustive;
mod facets;
pub mod index;
pub mod matcher;
mod pattern;
mod plan;
pub mod ranking;
mod repos;
mod select;
pub mod stream;

pub use basic::{BasicQuery, MAX_BASIC_QUERIES, TooManyBasicQueries, basic_queries};
pub use context::{
    ContextScope, ContextSpec, GLOBAL_CONTEXT, check_context_fields, context_of,
    substitute_context, validate_context_query,
};
pub use dates::{InvalidDate, parse_date};
pub use facets::{Facets, MAX_FILTERS_PER_KIND};
pub use matcher::Matcher;
pub use pattern::{CompileError, PatternSpec};
pub use plan::{
    CommitFilter, FileFilter, Job, Limits, PatternInfo, PlanError, RepoFilter, Visibility,
    YesNoOnly, parse_duration, plan_query,
};
pub use repos::{
    CatalogRepo, DEFAULT_PAGE_SIZE, RepoCatalog, RepoMatcher, RepoPage, RepoRevs, ResolveError,
};
pub use select::{SelectPath, Selector};
