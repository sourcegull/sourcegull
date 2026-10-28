//! Planner: query → job tree.
//!
//! The planner validates the query, expands it into basic queries and picks
//! a backend job per result type. Repo resolution and the split between
//! indexed and unindexed search happen when the jobs run; the jobs
//! here describe what to search, not where.
//!
//! Default result types: without `type:`, a pattern searches file contents,
//! paths and repository names (paths and names only when no file filter
//! narrows the search), and a query with filters but no pattern lists files
//! (with `file:`/`lang:`) or repositories.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_config::{SearchLimits, SiteConfigHandle};
use sourcegull_query::{
    Node, Parameter, ParseError, Pattern, PatternKind, PatternType, Predicate, Span,
    ValidationError, parse, validate,
};

use crate::basic::{BasicQuery, TooManyBasicQueries, basic_queries};
use crate::dates::{InvalidDate, parse_date};
use crate::select::SelectPath;

/// Limits from site config (`search.limits`). [`Limits::default`] matches the defaults of
/// an unset `search.limits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Result count without `count:`.
    pub default_count: usize,
    pub default_timeout: Duration,
    /// `timeout:` is clamped to this.
    pub max_timeout: Duration,
    /// Repositories one part of a search spans at most; `None` is no limit.
    pub max_repos: Option<usize>,
    /// Repositories a commit or diff search may span without `before:`/`after:`.
    pub commit_diff_max_repos: usize,
    /// ... and with one.
    pub commit_diff_with_time_filter_max_repos: usize,
    /// Results a search job collects at most.
    pub exhaustive_max_results: usize,
    /// How long a search job may search one repository revision.
    pub exhaustive_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits::from(&SearchLimits::default())
    }
}

impl From<&SearchLimits> for Limits {
    fn from(l: &SearchLimits) -> Self {
        let usize = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
        let secs = |n: u32| Duration::from_secs(u64::from(n.max(1)));
        Limits {
            default_count: usize(l.streaming_max_results),
            default_timeout: secs(l.default_timeout_seconds),
            max_timeout: secs(l.max_timeout_seconds),
            max_repos: l.max_repos.map(usize),
            commit_diff_max_repos: usize(l.commit_diff_max_repos),
            commit_diff_with_time_filter_max_repos: usize(l.commit_diff_with_time_filter_max_repos),
            exhaustive_max_results: usize(l.exhaustive_max_results),
            exhaustive_timeout: secs(l.exhaustive_timeout_seconds),
        }
    }
}

impl Limits {
    /// The limits of the current site config.
    pub fn from_site(config: &SiteConfigHandle) -> Self {
        Limits::from(&config.load().value.search_limits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum YesNoOnly {
    Yes,
    #[default]
    No,
    Only,
}

impl YesNoOnly {
    pub(crate) fn parse(v: &str) -> YesNoOnly {
        match v.to_ascii_lowercase().as_str() {
            "only" => YesNoOnly::Only,
            "yes" | "y" | "true" => YesNoOnly::Yes,
            _ => YesNoOnly::No,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Any,
    Public,
    Private,
}

/// Which repositories and revisions to search.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoFilter {
    /// Regexps on the repository name; all must match.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Revisions from `rev:` and `repo:name@rev:rev2`. Empty is the default branch.
    pub revs: Vec<String>,
    pub fork: YesNoOnly,
    pub archived: YesNoOnly,
    pub visibility: Visibility,
    pub context: Option<String>,
    /// Predicates with their negation (`-repo:has.file(...)`).
    pub predicates: Vec<(bool, Predicate)>,
}

/// Which files to search.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFilter {
    /// Regexps on the path; all must match.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub langs: Vec<String>,
    pub exclude_langs: Vec<String>,
    pub predicates: Vec<(bool, Predicate)>,
}

impl FileFilter {
    pub fn is_empty(&self) -> bool {
        self.include.is_empty()
            && self.exclude.is_empty()
            && self.langs.is_empty()
            && self.exclude_langs.is_empty()
            && self.predicates.is_empty()
    }
}

/// The pattern tree and how to match it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternInfo {
    pub pattern: Node,
    pub pattern_type: PatternType,
    pub case_sensitive: bool,
    /// Structural search: the `rule:` constraint.
    pub rule: Option<String>,
}

/// Filters for commit and diff searches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitFilter {
    pub authors: Vec<String>,
    pub exclude_authors: Vec<String>,
    pub committers: Vec<String>,
    pub exclude_committers: Vec<String>,
    pub messages: Vec<String>,
    pub exclude_messages: Vec<String>,
    pub before: Option<DateTime<Utc>>,
    pub after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// File contents. Structural queries carry `PatternType::Structural`.
    Content {
        pattern: PatternInfo,
        repos: RepoFilter,
        files: FileFilter,
        index: YesNoOnly,
    },
    /// File paths; without a pattern, lists the files that match the filters.
    Path {
        pattern: Option<PatternInfo>,
        repos: RepoFilter,
        files: FileFilter,
        index: YesNoOnly,
    },
    /// Repository names; without a pattern, lists repositories.
    Repo {
        pattern: Option<PatternInfo>,
        repos: RepoFilter,
    },
    Symbol {
        pattern: Option<PatternInfo>,
        repos: RepoFilter,
        files: FileFilter,
        index: YesNoOnly,
    },
    /// Commits (`diff: false`) or diffs. `files` keeps commits that change a
    /// matching file (and, for diffs, only those files).
    Commit {
        pattern: Option<PatternInfo>,
        repos: RepoFilter,
        files: FileFilter,
        filter: CommitFilter,
        diff: bool,
    },
    /// Runs children concurrently and merges their streams.
    Parallel(Vec<Job>),
    /// `select:` projection, e.g. `repo` or `symbol.function`.
    Select {
        path: String,
        child: Box<Job>,
    },
    /// Stops after `count` results; `None` is `count:all`.
    Limit {
        count: Option<usize>,
        child: Box<Job>,
    },
    Timeout {
        timeout: Duration,
        child: Box<Job>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("empty query")]
    Empty,
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error(transparent)]
    TooManyBasicQueries(#[from] TooManyBasicQueries),
    #[error("{field}: {source}")]
    Date {
        field: &'static str,
        source: InvalidDate,
        span: Span,
    },
}

/// Parses, validates and plans `input`.
pub fn plan_query(
    input: &str,
    default_pattern_type: PatternType,
    limits: &Limits,
    now: DateTime<Utc>,
) -> Result<Job, PlanError> {
    let query = parse(input, default_pattern_type)?;
    validate(&query)?;
    let mut basics = basic_queries(&query)?;
    if basics.is_empty() {
        return Err(PlanError::Empty);
    }
    if query.pattern_type == PatternType::Structural {
        basics = vec![structural_basic(input, &query)];
    }
    if matches!(
        query.pattern_type,
        PatternType::Standard | PatternType::Literal
    ) {
        for b in &mut basics {
            if let Some(p) = &mut b.pattern {
                keep_spacing(p, input);
            }
        }
    }
    let mut jobs = basics
        .iter()
        .map(|b| plan_basic(b, query.pattern_type, now))
        .collect::<Result<Vec<_>, _>>()?;
    let job = if jobs.len() == 1 {
        jobs.pop().expect("one job")
    } else {
        Job::Parallel(jobs)
    };

    let params = query.parameters();
    let last = |field: &str| {
        params
            .iter()
            .rev()
            .find(|p| p.field == field)
            .map(|p| p.value.as_str())
    };
    let count = match last("count") {
        Some(v) if v.eq_ignore_ascii_case("all") => None,
        Some(v) => v.parse().ok(),
        None => Some(limits.default_count),
    };
    let timeout = last("timeout")
        .and_then(parse_duration)
        .unwrap_or(limits.default_timeout)
        .min(limits.max_timeout);
    Ok(Job::Timeout {
        timeout,
        child: Box::new(Job::Limit {
            count,
            child: Box::new(job),
        }),
    })
}

/// Merges adjacent literal patterns of a standard or literal query that only whitespace
/// separates in `input` into one pattern, keeping that whitespace: `foo  bar` matches two
/// spaces and `foo<tab>bar` a tab. Patterns separated by anything else (a field between them)
/// stay apart, and the matcher joins them on one space.
fn keep_spacing(node: &mut Node, input: &str) {
    let Node::Operator { kind, operands, .. } = node else {
        return;
    };
    for operand in operands.iter_mut() {
        keep_spacing(operand, input);
    }
    if *kind != sourcegull_query::OperatorKind::Concat {
        return;
    }
    let mergeable = |p: &Pattern| !p.negated && p.kind != PatternKind::Regexp;
    let mut merged: Vec<Node> = Vec::with_capacity(operands.len());
    for operand in operands.drain(..) {
        if let (Some(Node::Pattern(prev)), Node::Pattern(next)) = (merged.last_mut(), &operand)
            && mergeable(prev)
            && mergeable(next)
            && let Some(gap) = input.get(prev.span.end..next.span.start)
            && !gap.is_empty()
            && gap.chars().all(char::is_whitespace)
        {
            prev.value.push_str(gap);
            prev.value.push_str(&next.value);
            prev.kind = PatternKind::Literal;
            prev.span.end = next.span.end;
            continue;
        }
        merged.push(operand);
    }
    *operands = merged;
    if operands.len() == 1 {
        *node = operands.pop().expect("one operand");
    }
}

/// A structural query as one basic query: its parameters, and the rest of
/// the input verbatim as the template. Parentheses and words such as `or`
/// and `not` are part of code, so the parser's reading of them as grouping
/// and operators does not apply.
fn structural_basic(input: &str, query: &sourcegull_query::Query) -> BasicQuery {
    let parameters: Vec<Parameter> = query.parameters().into_iter().cloned().collect();
    let mut spans: Vec<Span> = parameters.iter().map(|p| p.span).collect();
    spans.sort_by_key(|s| s.start);
    let mut parts = Vec::new();
    let mut at = 0;
    for s in spans {
        if s.start >= at {
            parts.push(&input[at..s.start]);
            at = s.end;
        }
    }
    parts.push(&input[at.min(input.len())..]);
    let template = parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let pattern = (!template.is_empty()).then_some(Node::Pattern(Pattern {
        value: template,
        kind: PatternKind::Literal,
        negated: false,
        span: Span {
            start: 0,
            end: input.len(),
        },
    }));
    BasicQuery {
        parameters,
        pattern,
    }
}

fn plan_basic(
    b: &BasicQuery,
    pattern_type: PatternType,
    now: DateTime<Utc>,
) -> Result<Job, PlanError> {
    let repos = repo_filter(b);
    let files = file_filter(b);
    let commits = commit_filter(b, now)?;
    let index = b
        .value("index")
        .map(YesNoOnly::parse)
        .unwrap_or(YesNoOnly::Yes);
    let case_sensitive = b
        .value("case")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "yes" | "y" | "true"));
    let rule = b.value("rule").map(str::to_owned);
    let info = |pattern: Node| PatternInfo {
        pattern,
        pattern_type,
        case_sensitive,
        rule: rule.clone(),
    };

    // `content:` searches file contents literally, whatever the pattern type, and is ANDed
    // with the pattern (a structural template stays out of it).
    let and = |a: Node, b: Node| Node::Operator {
        kind: sourcegull_query::OperatorKind::And,
        span: Span {
            start: a.span().start,
            end: b.span().end,
        },
        operands: vec![a, b],
    };
    let content = b
        .fields("content")
        .map(|p| {
            Node::Pattern(Pattern {
                value: p.value.clone(),
                kind: PatternKind::Quoted,
                negated: p.negated,
                span: p.span,
            })
        })
        .reduce(and);
    let pattern = b.pattern.clone().map(info);
    let content: Option<PatternInfo> = match (content, &pattern) {
        (None, _) => None,
        (Some(c), Some(p)) if pattern_type != PatternType::Structural => {
            Some(info(and(p.pattern.clone(), c)))
        }
        (Some(c), _) => Some(PatternInfo {
            pattern: c,
            pattern_type: PatternType::Literal,
            case_sensitive,
            rule: None,
        }),
    };

    let types: Vec<String> = b
        .fields("type")
        .map(|p| p.value.to_ascii_lowercase())
        .collect();
    let select = b.value("select").and_then(SelectPath::parse);
    let select_types = select
        .as_ref()
        .and_then(|s| s.default_types(pattern.is_some() || content.is_some()));
    let types: Vec<&str> = if let (true, Some(t)) = (types.is_empty(), select_types) {
        t.to_vec()
    } else if types.is_empty() {
        let narrowed = !files.is_empty() || content.is_some();
        match (&pattern, &content) {
            (_, Some(_)) => vec!["file"],
            (Some(_), None) if pattern_type == PatternType::Structural || narrowed => vec!["file"],
            (Some(_), None) => vec!["file", "path", "repo"],
            (None, None) if narrowed => vec!["path"],
            (None, None) => vec!["repo"],
        }
    } else {
        types.iter().map(String::as_str).collect()
    };

    let mut jobs = Vec::new();
    for t in types.into_iter() {
        let job = match t {
            "file" => match content.clone().or_else(|| pattern.clone()) {
                Some(pattern) => Job::Content {
                    pattern,
                    repos: repos.clone(),
                    files: files.clone(),
                    index,
                },
                None => Job::Path {
                    pattern: None,
                    repos: repos.clone(),
                    files: files.clone(),
                    index,
                },
            },
            "path" => Job::Path {
                pattern: pattern.clone(),
                repos: repos.clone(),
                files: files.clone(),
                index,
            },
            "repo" => Job::Repo {
                pattern: pattern.clone(),
                repos: repos.clone(),
            },
            "symbol" => Job::Symbol {
                pattern: pattern.clone(),
                repos: repos.clone(),
                files: files.clone(),
                index,
            },
            "commit" | "diff" => Job::Commit {
                pattern: pattern.clone(),
                repos: repos.clone(),
                files: files.clone(),
                filter: commits.clone(),
                diff: t == "diff",
            },
            _ => continue,
        };
        if !jobs.contains(&job) {
            jobs.push(job);
        }
    }
    let mut job = if jobs.len() == 1 {
        jobs.pop().expect("one job")
    } else {
        Job::Parallel(jobs)
    };
    if let Some(path) = b.value("select") {
        job = Job::Select {
            path: path.to_ascii_lowercase(),
            child: Box::new(job),
        };
    }
    Ok(job)
}

fn repo_filter(b: &BasicQuery) -> RepoFilter {
    let mut f = RepoFilter::default();
    for p in b.fields("repo") {
        if let Some(Ok(pred)) = p.predicate() {
            f.predicates.push((p.negated, pred));
            continue;
        }
        let (name, revs) = match p.value.split_once('@') {
            Some((name, revs)) if !p.negated => (name, Some(revs)),
            _ => (p.value.as_str(), None),
        };
        if let Some(revs) = revs {
            f.revs.extend(
                revs.split(':')
                    .filter(|r| !r.is_empty())
                    .map(str::to_string),
            );
        }
        if !name.is_empty() {
            if p.negated {
                &mut f.exclude
            } else {
                &mut f.include
            }
            .push(name.to_string());
        }
    }
    for p in b.fields("rev") {
        match p.predicate() {
            Some(Ok(pred)) => f.predicates.push((p.negated, pred)),
            _ => f.revs.extend(
                p.value
                    .split(':')
                    .filter(|r| !r.is_empty())
                    .map(str::to_string),
            ),
        }
    }
    f.fork = b.value("fork").map(YesNoOnly::parse).unwrap_or_default();
    f.archived = b
        .value("archived")
        .map(YesNoOnly::parse)
        .unwrap_or_default();
    f.visibility = match b
        .value("visibility")
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("public") => Visibility::Public,
        Some("private") => Visibility::Private,
        _ => Visibility::Any,
    };
    f.context = b.value("context").map(str::to_string);
    f
}

fn file_filter(b: &BasicQuery) -> FileFilter {
    let mut f = FileFilter::default();
    for p in b.fields("file") {
        match p.predicate() {
            Some(Ok(pred)) => f.predicates.push((p.negated, pred)),
            _ => if p.negated {
                &mut f.exclude
            } else {
                &mut f.include
            }
            .push(p.value.clone()),
        }
    }
    for p in b.fields("lang") {
        if p.negated {
            &mut f.exclude_langs
        } else {
            &mut f.langs
        }
        .push(p.value.clone());
    }
    f
}

fn commit_filter(b: &BasicQuery, now: DateTime<Utc>) -> Result<CommitFilter, PlanError> {
    let mut f = CommitFilter::default();
    let split = |field, include: &mut Vec<String>, exclude: &mut Vec<String>| {
        for p in b.fields(field) {
            if p.negated {
                &mut *exclude
            } else {
                &mut *include
            }
            .push(p.value.clone());
        }
    };
    split("author", &mut f.authors, &mut f.exclude_authors);
    split("committer", &mut f.committers, &mut f.exclude_committers);
    split("message", &mut f.messages, &mut f.exclude_messages);
    let date = |p: &Parameter| {
        parse_date(&p.value, now).map_err(|source| PlanError::Date {
            field: p.field,
            source,
            span: p.span,
        })
    };
    // Several bounds narrow the range: the earliest `before`, the latest `after`.
    for p in b.fields("before") {
        let t = date(p)?;
        f.before = Some(f.before.map_or(t, |b| b.min(t)));
    }
    for p in b.fields("after") {
        let t = date(p)?;
        f.after = Some(f.after.map_or(t, |a| a.max(t)));
    }
    Ok(f)
}

/// Go-style duration (`10s`, `1m30s`, `1.5h`, `500ms`).
pub fn parse_duration(s: &str) -> Option<Duration> {
    let mut rest = s;
    let mut total = 0f64;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let num = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let n: f64 = rest[..num].parse().ok()?;
        rest = &rest[num..];
        let (unit, secs) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("µs", 1e-6),
            ("ms", 1e-3),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
        ]
        .into_iter()
        .find(|(u, _)| rest.starts_with(u))?;
        rest = &rest[unit.len()..];
        total += n * secs;
    }
    Duration::try_from_secs_f64(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 3, 31, 12, 0, 0).unwrap()
    }

    fn plan(q: &str) -> Job {
        plan_query(q, PatternType::Keyword, &Limits::default(), now()).unwrap()
    }

    /// The job under the Timeout and Limit wrappers.
    fn inner(job: Job) -> Job {
        let Job::Timeout { child, .. } = job else {
            panic!("timeout")
        };
        let Job::Limit { child, .. } = *child else {
            panic!("limit")
        };
        *child
    }

    /// Short names of the backend jobs.
    fn kinds(job: &Job) -> Vec<&'static str> {
        match job {
            Job::Content { .. } => vec!["content"],
            Job::Path { .. } => vec!["path"],
            Job::Repo { .. } => vec!["repo"],
            Job::Symbol { .. } => vec!["symbol"],
            Job::Commit { diff: false, .. } => vec!["commit"],
            Job::Commit { diff: true, .. } => vec!["diff"],
            Job::Parallel(jobs) => jobs.iter().flat_map(kinds).collect(),
            Job::Select { child, .. } | Job::Limit { child, .. } | Job::Timeout { child, .. } => {
                kinds(child)
            }
        }
    }

    #[test]
    fn default_types() {
        assert_eq!(kinds(&plan("foo")), ["content", "path", "repo"]);
        assert_eq!(kinds(&plan("foo lang:go")), ["content"]);
        assert_eq!(kinds(&plan("file:\\.rs$")), ["path"]);
        assert_eq!(kinds(&plan("repo:gitlab")), ["repo"]);
        assert_eq!(kinds(&plan("repo:x content:TODO")), ["content"]);
        assert_eq!(
            kinds(&plan("foo type:symbol type:commit")),
            ["symbol", "commit"]
        );
        let structural = plan_query(
            "f(:[x])",
            PatternType::Structural,
            &Limits::default(),
            now(),
        )
        .unwrap();
        assert_eq!(kinds(&structural), ["content"]);
    }

    #[test]
    fn limits_come_from_the_site_config() {
        assert_eq!(Limits::default().default_count, 500);
        assert_eq!(Limits::default().default_timeout, Duration::from_secs(20));
        assert_eq!(Limits::default().max_timeout, Duration::from_secs(60));
        let limits = Limits::from(&SearchLimits {
            streaming_max_results: 3,
            default_timeout_seconds: 2,
            max_timeout_seconds: 5,
            ..SearchLimits::default()
        });
        let outer = |q: &str| match plan_query(q, PatternType::Keyword, &limits, now()).unwrap() {
            Job::Timeout { timeout, child } => match *child {
                Job::Limit { count, .. } => (timeout, count),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(outer("foo"), (Duration::from_secs(2), Some(3)));
        assert_eq!(
            outer("foo timeout:4s count:9"),
            (Duration::from_secs(4), Some(9))
        );
        // `timeout:` is clamped to the maximum.
        assert_eq!(outer("foo timeout:1m").0, Duration::from_secs(5));
    }

    #[test]
    fn structural_templates_are_verbatim() {
        let job = plan_query(
            "if (:[x]) { :[y] } lang:go rule:'where :[x] != \"\"' or not",
            PatternType::Structural,
            &Limits::default(),
            now(),
        )
        .unwrap();
        let Job::Content { pattern, files, .. } = inner(job) else {
            panic!()
        };
        let Node::Pattern(p) = &pattern.pattern else {
            panic!()
        };
        assert_eq!(p.value, "if (:[x]) { :[y] } or not");
        assert_eq!(pattern.rule.as_deref(), Some("where :[x] != \"\""));
        assert_eq!(files.langs, ["go"]);
        assert_eq!(
            crate::PatternSpec::from_pattern(&pattern).unwrap(),
            crate::PatternSpec::Structural {
                template: "if (:[x]) { :[y] } or not".into(),
                rule: Some("where :[x] != \"\"".into()),
            }
        );
    }

    #[test]
    fn content_fields_are_anded_with_the_pattern() {
        use crate::PatternSpec;
        let spec = |q: &str, ty: PatternType| {
            let job = plan_query(q, ty, &Limits::default(), now()).unwrap();
            let Job::Content { pattern, .. } = inner(job) else {
                panic!("{q}")
            };
            PatternSpec::from_pattern(&pattern).unwrap()
        };
        let atom = |regex: &str, negated: bool| PatternSpec::Atom {
            regex: regex.into(),
            negated,
        };
        assert_eq!(
            spec("foo -content:bar", PatternType::Keyword),
            PatternSpec::And(vec![atom("foo", false), atom("bar", true)])
        );
        // The content stays literal in a regexp query; the pattern does not.
        assert_eq!(
            spec("a.c content:\"x.y\"", PatternType::Regexp),
            PatternSpec::And(vec![atom("a.c", false), atom("x\\.y", false)])
        );
        assert_eq!(
            spec("content:a.c", PatternType::Regexp),
            atom("a\\.c", false)
        );
    }

    #[test]
    fn limits_and_timeout() {
        let Job::Timeout { timeout, child } = plan("foo") else {
            panic!()
        };
        assert_eq!(timeout, Duration::from_secs(20));
        assert!(matches!(
            *child,
            Job::Limit {
                count: Some(500),
                ..
            }
        ));
        let Job::Timeout { timeout, child } = plan("foo count:all timeout:5m") else {
            panic!()
        };
        assert_eq!(timeout, Duration::from_secs(60));
        assert!(matches!(*child, Job::Limit { count: None, .. }));
        let Job::Timeout { timeout, .. } = plan("foo timeout:1m30s") else {
            panic!()
        };
        assert_eq!(timeout, Duration::from_secs(60));
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("10"), None);
    }

    #[test]
    fn filters() {
        let job = inner(plan(
            "foo repo:^gitlab/ -repo:test repo:api@main:v1 rev:dev -file:_test\\.go lang:go case:yes fork:yes index:no",
        ));
        let Job::Content {
            pattern,
            repos,
            files,
            index,
        } = job
        else {
            panic!("{job:?}")
        };
        assert!(pattern.case_sensitive);
        assert_eq!(repos.include, ["^gitlab/", "api"]);
        assert_eq!(repos.exclude, ["test"]);
        assert_eq!(repos.revs, ["main", "v1", "dev"]);
        assert_eq!(repos.fork, YesNoOnly::Yes);
        assert_eq!(repos.archived, YesNoOnly::No);
        assert_eq!(files.exclude, ["_test\\.go"]);
        assert_eq!(files.langs, ["go"]);
        assert_eq!(index, YesNoOnly::No);
    }

    #[test]
    fn predicates_go_to_filters() {
        let job = inner(plan(
            "repo:has.file(path:go.mod) -repo:has.topic(archive) select:repo",
        ));
        let Job::Select { path, child } = job else {
            panic!()
        };
        assert_eq!(path, "repo");
        let Job::Repo { repos, .. } = *child else {
            panic!()
        };
        assert!(repos.include.is_empty());
        assert_eq!(repos.predicates.len(), 2);
        assert!(repos.predicates[1].0);
    }

    #[test]
    fn select_picks_types() {
        assert_eq!(kinds(&plan("foo select:symbol.function")), ["symbol"]);
        assert_eq!(kinds(&plan("foo select:file")), ["content", "path"]);
        assert_eq!(kinds(&plan("foo select:content")), ["content"]);
        assert_eq!(kinds(&plan("foo select:commit.diff.added")), ["diff"]);
        assert_eq!(kinds(&plan("foo select:repo")), ["content", "path", "repo"]);
        assert_eq!(kinds(&plan("foo type:commit select:repo")), ["commit"]);
    }

    #[test]
    fn diff_with_file_filters() {
        let Job::Commit {
            files, diff: true, ..
        } = inner(plan("type:diff file:\\.rs$ -lang:go fix"))
        else {
            panic!()
        };
        assert_eq!(files.include, ["\\.rs$"]);
        assert_eq!(files.exclude_langs, ["go"]);
    }

    #[test]
    fn commit_filters() {
        let job = inner(plan(
            "type:commit author:alice -author:bot after:\"2 weeks ago\" after:2024-03-20 before:yesterday fix",
        ));
        let Job::Commit {
            filter,
            diff: false,
            pattern: Some(_),
            ..
        } = job
        else {
            panic!()
        };
        assert_eq!(filter.authors, ["alice"]);
        assert_eq!(filter.exclude_authors, ["bot"]);
        assert_eq!(
            filter.after,
            Some(Utc.with_ymd_and_hms(2024, 3, 20, 0, 0, 0).unwrap())
        );
        assert_eq!(
            filter.before,
            Some(Utc.with_ymd_and_hms(2024, 3, 30, 0, 0, 0).unwrap())
        );
        let err = plan_query(
            "type:commit before:soon",
            PatternType::Keyword,
            &Limits::default(),
            now(),
        );
        assert!(matches!(
            err,
            Err(PlanError::Date {
                field: "before",
                ..
            })
        ));
    }

    #[test]
    fn or_of_filters_runs_in_parallel() {
        let job = inner(plan("(repo:a foo) or (repo:b bar) lang:go"));
        let Job::Parallel(jobs) = job else { panic!() };
        assert_eq!(jobs.len(), 2);
        let Job::Content { repos, files, .. } = &jobs[1] else {
            panic!()
        };
        assert_eq!(repos.include, ["b"]);
        assert_eq!(files.langs, ["go"]);
    }

    #[test]
    fn errors() {
        let p = |q| plan_query(q, PatternType::Keyword, &Limits::default(), now());
        assert_eq!(p("  "), Err(PlanError::Empty));
        assert!(matches!(p("foo)"), Err(PlanError::Parse(_))));
        assert!(matches!(p("case:maybe foo"), Err(PlanError::Validation(_))));
    }
}
