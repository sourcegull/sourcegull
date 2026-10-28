//! Exhaustive search for Search Jobs.
//!
//! [`plan_exhaustive`] accepts only the queries that can run as search jobs and plans
//! them for an exhaustive run: one basic query (no `or` between filter groups), no `select:`,
//! no file predicates, no structural search, no regexp `.*`, and only file, path, commit and
//! diff results. Without `type:`, a pattern searches file contents and paths (never
//! repository names, unlike an interactive search), a `content:` pattern contents only, and a
//! query without a pattern lists files. The plan has no result limit below
//! [`Limits::exhaustive_max_results`] and the timeout [`Limits::exhaustive_timeout`]; Search
//! Jobs runs it one repository revision at a time.

use chrono::{DateTime, Utc};
use sourcegull_query::{Node, PatternType, parse, validate};

use crate::basic::basic_queries;
use crate::plan::{FileFilter, Job, Limits, PlanError, YesNoOnly, plan_query};

/// Result types a search job can produce.
pub const SUPPORTED_TYPES: &[&str] = &["file", "path", "commit", "diff"];

/// Why a query cannot run as a search job. The messages are shown to users as they are.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExhaustiveError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("Search Jobs do not support structural search.")]
    Structural,
    #[error(
        "Search Jobs do not support AND/OR between groups of filters (for example \
         \"file:\\.go or file:\\.rs\"). Split the query into one search job per group."
    )]
    MultipleQueries,
    #[error("Search Jobs do not support select:.")]
    Select,
    #[error("Search Jobs do not support file predicates such as {0}.")]
    FilePredicate(String),
    #[error("Search Jobs do not support the regular expression .* on its own.")]
    MatchAll,
    #[error(
        "Search Jobs support only the result types file, path, commit and diff; the query \
         asks for {0}."
    )]
    UnsupportedTypes(String),
}

/// Checks that `input` can run as a search job and plans it (see the module docs).
pub fn plan_exhaustive(
    input: &str,
    default_pattern_type: PatternType,
    limits: &Limits,
    now: DateTime<Utc>,
) -> Result<Job, ExhaustiveError> {
    let query = parse(input, default_pattern_type).map_err(PlanError::from)?;
    validate(&query).map_err(PlanError::from)?;
    if query.pattern_type == PatternType::Structural {
        return Err(ExhaustiveError::Structural);
    }
    let basics = basic_queries(&query).map_err(PlanError::from)?;
    let b = match basics.as_slice() {
        [] => return Err(PlanError::Empty.into()),
        [b] => b,
        _ => return Err(ExhaustiveError::MultipleQueries),
    };
    if b.fields("select").next().is_some() {
        return Err(ExhaustiveError::Select);
    }
    if let Some(p) = b.fields("file").find(|p| p.predicate().is_some()) {
        return Err(ExhaustiveError::FilePredicate(format!("file:{}", p.value)));
    }
    if query.pattern_type == PatternType::Regexp
        && matches!(&b.pattern, Some(Node::Pattern(p)) if !p.negated && p.value == ".*")
    {
        return Err(ExhaustiveError::MatchAll);
    }
    let types: Vec<String> = b
        .fields("type")
        .filter(|p| !p.negated)
        .map(|p| p.value.to_ascii_lowercase())
        .collect();
    let unsupported: Vec<&str> = types
        .iter()
        .map(String::as_str)
        .filter(|t| !SUPPORTED_TYPES.contains(t))
        .collect();
    if !unsupported.is_empty() {
        return Err(ExhaustiveError::UnsupportedTypes(unsupported.join(", ")));
    }

    let planned = plan_query(input, default_pattern_type, limits, now)?;
    let mut leaves = Vec::new();
    flatten(strip(planned), &mut leaves);
    if types.is_empty() && b.fields("content").next().is_none() {
        let index = b
            .value("index")
            .map(YesNoOnly::parse)
            .unwrap_or(YesNoOnly::Yes);
        leaves = default_types(leaves, index);
    }
    let child = match leaves.len() {
        1 => leaves.pop().expect("one job"),
        _ => Job::Parallel(leaves),
    };
    let max = limits.exhaustive_max_results;
    let count = b
        .value("count")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(max, |n| n.min(max));
    Ok(Job::Timeout {
        timeout: limits.exhaustive_timeout,
        child: Box::new(Job::Limit {
            count: Some(count),
            child: Box::new(child),
        }),
    })
}

/// The result limit of a plan from [`plan_exhaustive`]; `None` when it has none.
pub fn max_results(job: &Job) -> Option<usize> {
    match job {
        Job::Timeout { child, .. } => max_results(child),
        Job::Limit { count, .. } => *count,
        _ => None,
    }
}

/// The plan without the interactive timeout and limit.
fn strip(job: Job) -> Job {
    match job {
        Job::Timeout { child, .. } | Job::Limit { child, .. } => strip(*child),
        other => other,
    }
}

fn flatten(job: Job, out: &mut Vec<Job>) {
    match job {
        Job::Parallel(jobs) => jobs.into_iter().for_each(|j| flatten(j, out)),
        other => out.push(other),
    }
}

/// Applies the exhaustive default result types (file and path) to an interactive plan made
/// without `type:`: repository name jobs go, a content search gets the path search of the
/// same pattern, and a query that only listed repositories lists their files instead.
fn default_types(leaves: Vec<Job>, index: YesNoOnly) -> Vec<Job> {
    let mut out: Vec<Job> = Vec::new();
    for job in leaves {
        match job {
            Job::Content {
                pattern,
                repos,
                files,
                index,
            } => {
                let path = Job::Path {
                    pattern: Some(pattern.clone()),
                    repos: repos.clone(),
                    files: files.clone(),
                    index,
                };
                out.push(Job::Content {
                    pattern,
                    repos,
                    files,
                    index,
                });
                if !out.contains(&path) {
                    out.push(path);
                }
            }
            Job::Repo {
                pattern: None,
                repos,
            } => {
                let path = Job::Path {
                    pattern: None,
                    repos,
                    files: FileFilter::default(),
                    index,
                };
                if !out.contains(&path) {
                    out.push(path);
                }
            }
            Job::Repo { .. } => {}
            other => {
                if !out.contains(&other) {
                    out.push(other);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn plan(q: &str) -> Result<Job, ExhaustiveError> {
        plan_exhaustive(q, PatternType::Keyword, &Limits::default(), Utc::now())
    }

    /// The leaf jobs' kinds.
    fn kinds(q: &str) -> Vec<&'static str> {
        let mut leaves = Vec::new();
        flatten(strip(plan(q).unwrap()), &mut leaves);
        leaves
            .iter()
            .map(|j| match j {
                Job::Content { .. } => "content",
                Job::Path {
                    pattern: Some(_), ..
                } => "path",
                Job::Path { pattern: None, .. } => "files",
                Job::Commit { diff: false, .. } => "commit",
                Job::Commit { diff: true, .. } => "diff",
                Job::Repo { .. } => "repo",
                Job::Symbol { .. } => "symbol",
                _ => "other",
            })
            .collect()
    }

    #[test]
    fn accepts_exhaustive_queries() {
        assert_eq!(kinds("foo"), ["content", "path"]);
        assert_eq!(kinds("repo:acme foo lang:go"), ["content", "path"]);
        assert_eq!(kinds("content:foo"), ["content"]);
        assert_eq!(kinds("repo:acme"), ["files"]);
        assert_eq!(kinds("repo:acme file:\\.go$"), ["files"]);
        assert_eq!(kinds("foo type:file"), ["content"]);
        assert_eq!(kinds("foo type:path"), ["path"]);
        assert_eq!(kinds("type:commit author:alice fix"), ["commit"]);
        assert_eq!(kinds("type:diff fix"), ["diff"]);
        assert_eq!(kinds("(foo or bar) repo:acme"), ["content", "path"]);
        assert_eq!(kinds("repo:acme@main:dev foo"), ["content", "path"]);
        assert!(plan("patterntype:regexp foo.*bar").is_ok());
        assert!(plan("repo:has.file(path:go.mod) foo").is_ok());
    }

    #[test]
    fn plans_without_interactive_limits() {
        let max = Some(1_000_000);
        let timeout = Duration::from_secs(3600);
        let job = plan("foo").unwrap();
        assert_eq!(max_results(&job), max);
        assert!(matches!(job, Job::Timeout { timeout: t, .. } if t == timeout));
        // `count:` lowers the limit; `count:all` and larger values keep the job's.
        assert_eq!(max_results(&plan("foo count:10").unwrap()), Some(10));
        assert_eq!(max_results(&plan("foo count:all").unwrap()), max);
        assert_eq!(max_results(&plan("foo count:99999999").unwrap()), max);
        // `timeout:` does not shorten it.
        assert!(matches!(
            plan("foo timeout:5s").unwrap(),
            Job::Timeout { timeout: t, .. } if t == timeout
        ));
    }

    #[test]
    fn uses_the_site_limits() {
        let limits = Limits {
            exhaustive_max_results: 7,
            exhaustive_timeout: Duration::from_secs(9),
            ..Limits::default()
        };
        let job =
            plan_exhaustive("foo count:all", PatternType::Keyword, &limits, Utc::now()).unwrap();
        assert_eq!(max_results(&job), Some(7));
        assert!(matches!(job, Job::Timeout { timeout, .. } if timeout == Duration::from_secs(9)));
    }

    #[test]
    fn rejects_what_cannot_be_exhaustive() {
        assert_eq!(plan(""), Err(ExhaustiveError::Plan(PlanError::Empty)));
        assert!(matches!(plan("foo )"), Err(ExhaustiveError::Plan(_))));
        assert_eq!(
            plan("(file:\\.go foo) or (file:\\.rs bar)"),
            Err(ExhaustiveError::MultipleQueries)
        );
        assert_eq!(plan("foo select:repo"), Err(ExhaustiveError::Select));
        assert_eq!(
            plan("foo file:has.content(bar)"),
            Err(ExhaustiveError::FilePredicate(
                "file:has.content(bar)".into()
            ))
        );
        assert_eq!(
            plan("foo -file:has.owner(alice)"),
            Err(ExhaustiveError::FilePredicate(
                "file:has.owner(alice)".into()
            ))
        );
        assert_eq!(
            plan("patterntype:regexp .*"),
            Err(ExhaustiveError::MatchAll)
        );
        assert_eq!(
            plan("patterntype:structural fmt.Println(:[x])"),
            Err(ExhaustiveError::Structural)
        );
        assert_eq!(
            plan_exhaustive(
                "foo(:[x])",
                PatternType::Structural,
                &Limits::default(),
                Utc::now()
            ),
            Err(ExhaustiveError::Structural)
        );
        assert_eq!(
            plan("foo type:repo"),
            Err(ExhaustiveError::UnsupportedTypes("repo".into()))
        );
        assert_eq!(
            plan("foo type:symbol type:file"),
            Err(ExhaustiveError::UnsupportedTypes("symbol".into()))
        );
    }
}
