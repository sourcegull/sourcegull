//! Drill-down queries of a point:
//!
//! - `diffQuery`: the series query as a `type:diff` search bounded to the point's period,
//!   `after:` the previous point (none for the first) and `before:` the point, so it shows
//!   the changes that moved the value;
//! - `pointInTimeQuery`: the series query at `rev:at.time(<point>)`, the results the point
//!   counted.
//!
//! Both are restricted like the points: to the series' repository list (`repo:^(a|b)$`), its
//! repository query (prepended as its own group), and the view's include and exclude
//! filters and search context. For a capture group series, the value's own series replaces
//! the capture group with the captured value, so the search shows only that value.

use chrono::{DateTime, SecondsFormat, Utc};
use sourcegull_query::{Node, OperatorKind, PatternType, parse};

use crate::series::GenerationMethod;

/// What a point's drill-down queries are built from.
#[derive(Debug, Clone, Default)]
pub struct DrillDown<'a> {
    pub query: &'a str,
    pub method: Option<GenerationMethod>,
    /// The captured value of a capture group series' value series.
    pub capture: Option<&'a str>,
    pub repositories: &'a [String],
    pub repository_criteria: Option<&'a str>,
    pub include_repo_regex: Option<&'a str>,
    pub exclude_repo_regex: Option<&'a str>,
    /// One search context (the first of the view's).
    pub context: Option<&'a str>,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Quotes a filter value that holds whitespace or quotes (balanced parentheses stay in a
/// bare value).
fn value(v: &str) -> String {
    if v.is_empty() || v.contains(char::is_whitespace) || v.starts_with(['"', '\'']) {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_owned()
    }
}

fn regex_escape(s: &str) -> String {
    regex::escape(s)
}

impl DrillDown<'_> {
    fn pattern_type(&self) -> PatternType {
        match self.method {
            Some(GenerationMethod::MappingCompute) => PatternType::Regexp,
            _ => PatternType::Keyword,
        }
    }

    /// The repository filters every drill-down starts with.
    fn repo_filters(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(r) = self.include_repo_regex.filter(|r| !r.is_empty()) {
            out.push(format!("repo:{}", value(r)));
        }
        if let Some(r) = self.exclude_repo_regex.filter(|r| !r.is_empty()) {
            out.push(format!("-repo:{}", value(r)));
        }
        if !self.repositories.is_empty() {
            let names: Vec<String> = self.repositories.iter().map(|r| regex_escape(r)).collect();
            out.push(format!(
                "repo:{}",
                value(&format!("^({})$", names.join("|")))
            ));
        }
        if let Some(c) = self.context.filter(|c| !c.is_empty()) {
            out.push(format!("context:{}", value(c)));
        }
        out
    }

    /// The series query without its `type:` filters (the drill-down sets its own), with the
    /// capture group replaced by the captured value, grouped when it has `OR`.
    fn body(&self) -> String {
        let pt = self.pattern_type();
        let mut q = self.query.to_owned();
        if let Some(v) = self.capture {
            q = replace_capture(&q, pt, v);
        }
        let Ok(parsed) = parse(&q, pt) else {
            return q;
        };
        let mut cuts: Vec<(usize, usize)> = parsed
            .parameters()
            .iter()
            .filter(|p| p.field == "type")
            .map(|p| (p.span.start, p.span.end))
            .collect();
        cuts.sort_unstable();
        for (start, end) in cuts.into_iter().rev() {
            q.replace_range(start..end, "");
        }
        let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
        if parsed.root.as_ref().is_some_and(has_or) {
            format!("({q})")
        } else {
            q
        }
    }

    fn assemble(&self, mut parts: Vec<String>) -> String {
        let body = self.body();
        if !body.is_empty() {
            parts.push(body);
        }
        if self.method == Some(GenerationMethod::MappingCompute)
            && !self.query.to_ascii_lowercase().contains("patterntype:")
        {
            parts.push("patterntype:regexp".into());
        }
        let q = parts.join(" ");
        match self.repository_criteria.filter(|c| !c.trim().is_empty()) {
            Some(c) => {
                let c = c.trim();
                let grouped = parse(c, PatternType::Keyword)
                    .ok()
                    .and_then(|p| p.root)
                    .is_some_and(|r| has_or(&r));
                if grouped {
                    format!("({c}) {q}")
                } else {
                    format!("{c} {q}")
                }
            }
            None => q,
        }
    }

    /// The `type:diff` search for the period `(after, before]`.
    pub fn diff_query(&self, after: Option<DateTime<Utc>>, before: DateTime<Utc>) -> String {
        let mut parts = self.repo_filters();
        if let Some(a) = after {
            parts.push(format!("after:{}", rfc3339(a)));
        }
        parts.push(format!("before:{}", rfc3339(before)));
        parts.push("type:diff".into());
        self.assemble(parts)
    }

    /// The search at the point's time.
    pub fn point_in_time_query(&self, at: DateTime<Utc>) -> String {
        let mut parts = self.repo_filters();
        parts.push(format!("rev:at.time({})", rfc3339(at)));
        // `type:` is dropped from the body; keep the series' own result type.
        if let Ok(p) = parse(self.query, self.pattern_type())
            && let Some(t) = p.parameters().iter().find(|p| p.field == "type")
        {
            parts.push(format!("type:{}", t.value));
        }
        self.assemble(parts)
    }
}

fn has_or(n: &Node) -> bool {
    match n {
        Node::Operator { kind, operands, .. } => {
            *kind == OperatorKind::Or || operands.iter().any(has_or)
        }
        _ => false,
    }
}

/// `query` with the first capture group of its first regular expression pattern replaced by
/// the escaped `value` (`TODO\((\w+)\)` with `alice` gives `TODO\((alice)\)`); unchanged
/// when there is none.
pub fn replace_capture(query: &str, pt: PatternType, value: &str) -> String {
    let Ok(parsed) = parse(query, pt) else {
        return query.to_owned();
    };
    let mut patterns = Vec::new();
    if let Some(root) = &parsed.root {
        collect_patterns(root, &mut patterns);
    }
    for (start, end) in patterns {
        let src = &query[start..end];
        if let Some((gs, ge)) = first_capture_group(src) {
            let mut out = String::with_capacity(query.len() + value.len());
            out.push_str(&query[..start + gs + 1]);
            out.push_str(&regex_escape(value));
            out.push_str(&query[start + ge..]);
            return out;
        }
    }
    query.to_owned()
}

fn collect_patterns(n: &Node, out: &mut Vec<(usize, usize)>) {
    match n {
        Node::Pattern(p) if !p.negated => out.push((p.span.start, p.span.end)),
        Node::Operator { operands, .. } => operands.iter().for_each(|o| collect_patterns(o, out)),
        _ => {}
    }
}

/// The byte range of the first capturing group in a regexp, from its `(` to its `)`.
fn first_capture_group(re: &str) -> Option<(usize, usize)> {
    let b = re.as_bytes();
    let mut i = 0;
    let mut in_class = false;
    let mut open: Option<(usize, usize)> = None; // (start, depth at start)
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => {
                depth += 1;
                let rest = &re[i + 1..];
                // `(?:…)` and flags do not capture; `(?P<name>…)` and `(?<name>…)` do.
                let capturing = !rest.starts_with('?')
                    || rest.starts_with("?P<")
                    || (rest.starts_with("?<")
                        && !rest.starts_with("?<=")
                        && !rest.starts_with("?<!"));
                if capturing && open.is_none() {
                    let body = if rest.starts_with('?') {
                        i + 1 + rest.find('>')?
                    } else {
                        i
                    };
                    open = Some((body, depth));
                }
            }
            b')' if !in_class => {
                if let Some((start, d)) = open
                    && d == depth
                {
                    return Some((start, i));
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sourcegull_search::{Job, Limits, plan_query};

    fn at(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn diff_queries_bound_the_period_and_restrict_the_repositories() {
        let repos = vec!["gitlab.example.com/acme/web".to_owned()];
        let d = DrillDown {
            query: "TODO",
            repositories: &repos,
            include_repo_regex: Some("acme"),
            exclude_repo_regex: Some("secret"),
            ..DrillDown::default()
        };
        assert_eq!(
            d.diff_query(Some(at(1)), at(2)),
            "repo:acme -repo:secret repo:^(gitlab\\.example\\.com/acme/web)$ \
             after:2026-03-01T12:00:00Z before:2026-03-02T12:00:00Z type:diff TODO"
        );
        assert_eq!(
            DrillDown {
                query: "TODO",
                ..DrillDown::default()
            }
            .diff_query(None, at(2)),
            "before:2026-03-02T12:00:00Z type:diff TODO"
        );
        // The query plans as a diff search with both bounds.
        let q = d.diff_query(Some(at(1)), at(2));
        let job = plan_query(&q, PatternType::Keyword, &Limits::default(), Utc::now()).unwrap();
        fn find_diff(j: &Job) -> bool {
            match j {
                Job::Commit { diff, filter, .. } => {
                    *diff && filter.after.is_some() && filter.before.is_some()
                }
                Job::Timeout { child, .. } | Job::Limit { child, .. } => find_diff(child),
                Job::Parallel(js) => js.iter().all(find_diff),
                _ => false,
            }
        }
        assert!(find_diff(&job), "{job:?}");
    }

    #[test]
    fn type_filters_or_and_criteria_are_kept_apart() {
        let d = DrillDown {
            query: "type:commit fix or bug",
            repository_criteria: Some("repo:a or repo:b"),
            ..DrillDown::default()
        };
        assert_eq!(
            d.diff_query(None, at(2)),
            "(repo:a or repo:b) before:2026-03-02T12:00:00Z type:diff (fix or bug)"
        );
        assert_eq!(
            d.point_in_time_query(at(2)),
            "(repo:a or repo:b) rev:at.time(2026-03-02T12:00:00Z) type:commit (fix or bug)"
        );
    }

    #[test]
    fn capture_groups_are_replaced_by_the_value() {
        assert_eq!(
            replace_capture(r"TODO\((\w+)\)", PatternType::Regexp, "a.b"),
            r"TODO\((a\.b)\)"
        );
        assert_eq!(
            replace_capture(r"repo:^(x|y)$ (?:v)(?P<n>\d+)", PatternType::Regexp, "7"),
            r"repo:^(x|y)$ (?:v)(?P<n>7)"
        );
        assert_eq!(
            replace_capture(r"[(]a(b)", PatternType::Regexp, "z"),
            r"[(]a(z)"
        );
        assert_eq!(replace_capture("plain", PatternType::Regexp, "z"), "plain");
        let d = DrillDown {
            query: r"TODO\((\w+)\)",
            method: Some(GenerationMethod::MappingCompute),
            capture: Some("alice"),
            ..DrillDown::default()
        };
        assert_eq!(
            d.diff_query(None, at(2)),
            r"before:2026-03-02T12:00:00Z type:diff TODO\((alice)\) patterntype:regexp"
        );
    }
}
