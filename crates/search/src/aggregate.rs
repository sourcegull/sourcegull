//! Search aggregations: result counts grouped by
//! repository, path, commit author or the value of a regexp capture group,
//! computed live from a search's results (nothing is stored), with a
//! drill-down query per group.
//!
//! Which modes a query supports: paths need file results, authors need
//! commit or diff results, capture groups need a regexp pattern with a
//! capture group. Counts are match counts: ranges in
//! content results, symbols in symbol results, 1 for other results.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use regex::{Regex, RegexBuilder};
use sourcegull_query::{Node, PatternKind, PatternType};

use crate::stream::Match;
use crate::{Job, Limits, PatternInfo, SelectPath, plan_query};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregationMode {
    Repo,
    Path,
    Author,
    CaptureGroup,
}

impl AggregationMode {
    pub const ALL: [AggregationMode; 4] = [
        AggregationMode::Repo,
        AggregationMode::Path,
        AggregationMode::Author,
        AggregationMode::CaptureGroup,
    ];
}

/// A regexp pattern's first capture group: the compiled pattern, and where
/// the group is in the query text (for drill-down queries).
#[derive(Debug, Clone)]
struct Capture {
    regex: Regex,
    /// Byte range of the capture group in the query.
    group: std::ops::Range<usize>,
}

/// What a query searches, for deciding which modes apply.
#[derive(Debug, Clone)]
pub struct QueryShape {
    query: String,
    files: bool,
    commits: bool,
    repos: bool,
    select: Option<SelectPath>,
    capture: Option<Capture>,
}

impl QueryShape {
    /// Plans `query`; the error is the planner's message.
    pub fn new(
        query: &str,
        pattern_type: PatternType,
        now: DateTime<Utc>,
    ) -> Result<QueryShape, String> {
        let job =
            plan_query(query, pattern_type, &Limits::default(), now).map_err(|e| e.to_string())?;
        let mut shape = QueryShape {
            query: query.to_owned(),
            files: false,
            commits: false,
            repos: false,
            select: None,
            capture: None,
        };
        let mut patterns = Vec::new();
        shape.walk(&job, &mut patterns);
        // A capture group needs exactly one regexp pattern.
        if let [p] = &patterns[..] {
            shape.capture = capture(query, p);
        }
        Ok(shape)
    }

    fn walk<'j>(&mut self, job: &'j Job, patterns: &mut Vec<&'j PatternInfo>) {
        match job {
            Job::Content { pattern, .. } => {
                self.files = true;
                patterns.push(pattern);
            }
            Job::Path { .. } | Job::Symbol { .. } => self.files = true,
            Job::Repo { .. } => self.repos = true,
            Job::Commit { .. } => self.commits = true,
            Job::Parallel(jobs) => jobs.iter().for_each(|j| self.walk(j, patterns)),
            Job::Select { path, child } => {
                self.select = SelectPath::parse(path);
                self.walk(child, patterns);
            }
            Job::Limit { child, .. } | Job::Timeout { child, .. } => self.walk(child, patterns),
        }
    }

    /// Whether `mode` applies; the error says why not.
    pub fn check(&self, mode: AggregationMode) -> Result<(), &'static str> {
        let selects_repo = self.select == Some(SelectPath::Repo);
        match mode {
            AggregationMode::Repo => Ok(()),
            AggregationMode::Path if self.files && !selects_repo => Ok(()),
            AggregationMode::Path => Err("This query does not return file results."),
            AggregationMode::Author if self.commits && !self.files && !self.repos => Ok(()),
            AggregationMode::Author => Err("Authors apply to type:commit and type:diff queries."),
            AggregationMode::CaptureGroup if self.capture.is_some() && !selects_repo => Ok(()),
            AggregationMode::CaptureGroup => {
                Err("This query has no regular expression with a capture group.")
            }
        }
    }

    /// The mode used when none is asked for: a capture group, else authors,
    /// else repositories.
    pub fn default_mode(&self) -> AggregationMode {
        [AggregationMode::CaptureGroup, AggregationMode::Author]
            .into_iter()
            .find(|m| self.check(*m).is_ok())
            .unwrap_or(AggregationMode::Repo)
    }
}

/// The first capture group of the query's regexp pattern.
fn capture(query: &str, p: &PatternInfo) -> Option<Capture> {
    let Node::Pattern(pat) = &p.pattern else {
        return None;
    };
    let regexp = pat.kind == PatternKind::Regexp
        || (p.pattern_type == PatternType::Regexp && pat.kind != PatternKind::Quoted);
    if !regexp || pat.negated {
        return None;
    }
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(&pat.value)
        .ok()?;
    let group = first_capture(&ast)?;
    // The pattern's text in the query: `/regexp/` in standard queries.
    let text = query.get(pat.span.start..pat.span.end)?;
    let offset = pat.span.start + text.find(pat.value.as_str())?;
    let regex = RegexBuilder::new(&pat.value)
        .case_insensitive(!p.case_sensitive)
        .multi_line(true)
        .build()
        .ok()?;
    Some(Capture {
        regex,
        group: offset + group.start..offset + group.end,
    })
}

fn first_capture(ast: &regex_syntax::ast::Ast) -> Option<std::ops::Range<usize>> {
    use regex_syntax::ast::{Ast, GroupKind};
    match ast {
        Ast::Group(g) => match g.kind {
            GroupKind::CaptureIndex(_) | GroupKind::CaptureName { .. } => {
                Some(g.span.start.offset..g.span.end.offset)
            }
            GroupKind::NonCapturing(_) => first_capture(&g.ast),
        },
        Ast::Concat(c) => c.asts.iter().find_map(first_capture),
        Ast::Alternation(a) => a.asts.iter().find_map(first_capture),
        Ast::Repetition(r) => first_capture(&r.ast),
        _ => None,
    }
}

/// One group of an aggregation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub label: String,
    pub count: u64,
    /// A query for this group's results.
    pub query: String,
}

/// Aggregated results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregation {
    pub groups: Vec<Group>,
    /// Matches in groups beyond the limit.
    pub other_result_count: u64,
    /// Groups beyond the limit.
    pub other_group_count: u64,
}

/// Counts results into groups.
#[derive(Debug)]
pub struct Aggregator {
    mode: AggregationMode,
    shape: QueryShape,
    counts: HashMap<String, u64>,
}

impl Aggregator {
    pub fn new(shape: QueryShape, mode: AggregationMode) -> Aggregator {
        Aggregator {
            mode,
            shape,
            counts: HashMap::new(),
        }
    }

    fn count(&mut self, label: &str, n: u64) {
        if n == 0 {
            return;
        }
        match self.counts.get_mut(label) {
            Some(c) => *c += n,
            None => {
                self.counts.insert(label.to_owned(), n);
            }
        }
    }

    pub fn add(&mut self, m: &Match) {
        match self.mode {
            AggregationMode::Repo => {
                let (repo, n) = match m {
                    Match::Content { repository, .. }
                    | Match::Path { repository, .. }
                    | Match::Symbol { repository, .. }
                    | Match::Commit { repository, .. }
                    | Match::Repo { repository, .. } => (repository, result_count(m)),
                    Match::Person { .. } | Match::Team { .. } => return,
                };
                self.count(&repo.clone(), n);
            }
            AggregationMode::Path => {
                if let Match::Content { path, .. }
                | Match::Path { path, .. }
                | Match::Symbol { path, .. } = m
                {
                    self.count(&path.clone(), result_count(m));
                }
            }
            AggregationMode::Author => {
                if let Match::Commit { author_name, .. } = m {
                    self.count(&author_name.clone(), 1);
                }
            }
            AggregationMode::CaptureGroup => {
                let Some(cap) = &self.shape.capture else {
                    return;
                };
                let Match::Content { chunk_matches, .. } = m else {
                    return;
                };
                let mut values = Vec::new();
                for c in chunk_matches {
                    for r in &c.ranges {
                        let from = (r.start.offset - c.content_start.offset) as usize;
                        let to = (r.end.offset - c.content_start.offset) as usize;
                        let Some(text) = c.content.get(from..to) else {
                            continue;
                        };
                        if let Some(v) = cap.regex.captures(text).and_then(|c| c.get(1)) {
                            values.push(v.as_str().to_owned());
                        }
                    }
                }
                for v in values {
                    self.count(&v, 1);
                }
            }
        }
    }

    /// The `limit` largest groups (ties by label) and what is left over.
    pub fn finish(self, limit: usize) -> Aggregation {
        let mut groups: Vec<(String, u64)> = self.counts.into_iter().collect();
        groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let rest = groups.split_off(groups.len().min(limit));
        Aggregation {
            groups: groups
                .into_iter()
                .map(|(label, count)| Group {
                    query: drill_down(&self.shape, self.mode, &label),
                    label,
                    count,
                })
                .collect(),
            other_result_count: rest.iter().map(|g| g.1).sum(),
            other_group_count: rest.len() as u64,
        }
    }
}

fn result_count(m: &Match) -> u64 {
    match m {
        Match::Content { chunk_matches, .. } => chunk_matches
            .iter()
            .map(|c| c.ranges.len() as u64)
            .sum::<u64>()
            .max(1),
        Match::Symbol { symbols, .. } => symbols.len() as u64,
        _ => 1,
    }
}

/// A filter value, quoted when it holds whitespace or quotes.
fn quote(v: &str) -> String {
    if v.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_owned()
    }
}

/// The query for one group's results.
fn drill_down(shape: &QueryShape, mode: AggregationMode, label: &str) -> String {
    let q = &shape.query;
    let exact = format!("^{}$", regex::escape(label));
    match mode {
        AggregationMode::Repo => format!("{q} repo:{}", quote(&exact)),
        AggregationMode::Path => format!("{q} file:{}", quote(&exact)),
        AggregationMode::Author => format!("{q} author:{}", quote(&exact)),
        AggregationMode::CaptureGroup => match &shape.capture {
            Some(c) => format!(
                "{}{}{}",
                &q[..c.group.start],
                regex::escape(label),
                &q[c.group.end..]
            ),
            None => q.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{ChunkMatch, Location, Range};

    fn shape(q: &str) -> QueryShape {
        QueryShape::new(q, PatternType::Keyword, Utc::now()).unwrap()
    }

    fn content(repo: &str, path: &str, line: &str, ranges: &[(u32, u32)]) -> Match {
        let loc = |offset| Location {
            offset,
            line: 0,
            column: offset,
        };
        Match::Content {
            path: path.into(),
            repository_id: 1,
            repository: repo.into(),
            commit: "c".into(),
            branches: Vec::new(),
            chunk_matches: vec![ChunkMatch {
                content: line.into(),
                content_start: loc(0),
                ranges: ranges
                    .iter()
                    .map(|&(s, e)| Range {
                        start: loc(s),
                        end: loc(e),
                    })
                    .collect(),
            }],
            language: None,
        }
    }

    #[test]
    fn modes_per_query() {
        let s = shape("foo");
        assert!(s.check(AggregationMode::Repo).is_ok());
        assert!(s.check(AggregationMode::Path).is_ok());
        assert!(s.check(AggregationMode::Author).is_err());
        assert!(s.check(AggregationMode::CaptureGroup).is_err());
        assert_eq!(s.default_mode(), AggregationMode::Repo);
        let s = shape("type:commit fix");
        assert!(s.check(AggregationMode::Author).is_ok());
        assert!(s.check(AggregationMode::Path).is_err());
        assert_eq!(s.default_mode(), AggregationMode::Author);
        let s = shape("patterntype:regexp (\\w+)Reader");
        assert_eq!(s.default_mode(), AggregationMode::CaptureGroup);
        assert!(
            shape("patterntype:regexp func \\w+")
                .check(AggregationMode::CaptureGroup)
                .is_err()
        );
        assert!(
            shape("foo select:repo")
                .check(AggregationMode::Path)
                .is_err()
        );
        assert!(QueryShape::new(")", PatternType::Keyword, Utc::now()).is_err());
    }

    #[test]
    fn counts_by_repo_and_path() {
        let mut a = Aggregator::new(shape("foo"), AggregationMode::Repo);
        a.add(&content("r/a", "x.go", "foo foo", &[(0, 3), (4, 7)]));
        a.add(&content("r/b", "y.go", "foo", &[(0, 3)]));
        a.add(&content("r/c", "z.go", "foo", &[(0, 3)]));
        let agg = a.finish(2);
        assert_eq!(agg.groups[0].label, "r/a");
        assert_eq!(agg.groups[0].count, 2);
        assert_eq!(agg.groups[0].query, "foo repo:^r/a$");
        assert_eq!(agg.groups[1].label, "r/b");
        assert_eq!((agg.other_result_count, agg.other_group_count), (1, 1));

        let mut a = Aggregator::new(shape("foo"), AggregationMode::Path);
        a.add(&content("r/a", "dir/my file.go", "foo", &[(0, 3)]));
        assert_eq!(
            a.finish(10).groups[0].query,
            "foo file:\"^dir/my file\\\\.go$\""
        );
    }

    #[test]
    fn capture_groups_and_drill_down() {
        let q = "patterntype:regexp lang:go func\\s(\\w+)Reader";
        let mut a = Aggregator::new(shape(q), AggregationMode::CaptureGroup);
        let line = "func NewReader( func LimitReader( func NewReader(";
        a.add(&content("r", "a.go", line, &[(0, 14), (16, 32), (34, 48)]));
        let agg = a.finish(10);
        assert_eq!(agg.groups[0].label, "New");
        assert_eq!(agg.groups[0].count, 2);
        assert_eq!(
            agg.groups[0].query,
            "patterntype:regexp lang:go func\\sNewReader"
        );
        assert_eq!(agg.groups[1].label, "Limit");
        // Standard queries put the regexp between slashes.
        let q = "/err\\s(\\w+)/ lang:go";
        let s = QueryShape::new(q, PatternType::Standard, Utc::now()).unwrap();
        assert!(s.check(AggregationMode::CaptureGroup).is_ok());
        assert_eq!(
            drill_down(&s, AggregationMode::CaptureGroup, "a.b"),
            "/err\\sa\\.b/ lang:go"
        );
    }
}
