//! sourcegull-compute: Compute.
//!
//! Computes values from search results: a [`ComputeQuery`] is a search
//! query whose pattern is a regexp (match only) or a command
//! (`content:replace(…)`, `content:output(…)`, see [`query`]). The search
//! runs as usual with the viewer's permissions (in `sourcegull-http-api`,
//! `/.api/compute/stream` and GraphQL `compute`); [`Command::run`] turns
//! each search result into [`ComputeResult`]s. Templates are in
//! [`template`].
//!
//! Trends use [`Command::group_by`] and count values per file
//! with [`Command::count_file`] ([`count`]).

pub mod command;
pub mod count;
pub mod query;
pub mod result;
pub mod template;

pub use command::{Command, MatchPattern, OutputKind};
pub use count::{GroupBy, ValueCounts};
pub use query::{ComputeError, ComputeQuery, RegexpSite};
pub use result::{ComputeMatch, ComputeResult, MatchContext, Origin, Text};
pub use template::{MetaEnv, Template};
