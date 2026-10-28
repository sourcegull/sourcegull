//! Compute queries.
//!
//! A compute query is a search query whose one pattern is either a plain
//! regexp (match only: report the matches and their capture groups) or a
//! command written as a `content:` predicate:
//!
//! - `content:replace(pattern -> template)` (also `replace.regexp`,
//!   `replace.structural`): rewrite each matching file;
//! - `content:output(pattern -> template)` (also `output.regexp`,
//!   `output.structural`, `output.extra`): one value per match, joined by
//!   the separator. `output.extra` also returns the repository. A
//!   `separator:` term sets the separator (default newline); `type:` and
//!   `select:` change what the template runs over (see [`crate::command`]).
//!
//! The command is cut out of the query text before the rest is parsed with the
//! search query parser, so regexps with spaces and parentheses survive.

use regex::Regex;
use sourcegull_query::{Node, OperatorKind, PatternType};

use crate::command::{Command, MatchPattern, OutputKind};
use crate::template::Template;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComputeError {
    #[error("a compute query needs a search pattern or a content: command")]
    EmptyPattern,
    #[error("the pattern of a compute query cannot be negated")]
    Negated,
    #[error(
        "a compute query takes a single pattern or command; and, or, not and negation are not supported"
    )]
    Expression,
    #[error("a compute command takes \"PATTERN -> TEMPLATE\"; no \"->\" found")]
    Arrow,
    #[error("invalid regexp in the {0}: {1}")]
    Regexp(RegexpSite, String),
    #[error("invalid structural pattern: {0}")]
    Structural(String),
    #[error("invalid query: {0}")]
    Parse(String),
}

/// Where in a compute query a regexp that failed to compile was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexpSite {
    /// The pattern of a match-only query.
    Pattern,
    /// The pattern of a `content:replace…` command.
    Replace,
    /// The pattern of a `content:output…` command.
    Output,
}

impl std::fmt::Display for RegexpSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RegexpSite::Pattern => "search pattern",
            RegexpSite::Replace => "replacement pattern",
            RegexpSite::Output => "output pattern",
        })
    }
}

/// The command names a `content:` predicate may call.
const COMMANDS: &[&str] = &[
    "replace",
    "replace.regexp",
    "replace.structural",
    "output",
    "output.regexp",
    "output.structural",
    "output.extra",
];

/// A parsed compute query.
#[derive(Debug, Clone)]
pub struct ComputeQuery {
    pub command: Command,
    /// The search query's parameters (everything but the pattern), in the
    /// canonical form of the query parser.
    pub parameters: Vec<String>,
}

impl ComputeQuery {
    /// Parses a compute query.
    pub fn parse(input: &str) -> Result<ComputeQuery, ComputeError> {
        let (rest, separator) = take_separator(input);
        let (rest, predicate) = take_command(&rest);
        let query = sourcegull_query::parse(&rest, PatternType::Regexp)
            .map_err(|e| ComputeError::Parse(e.to_string()))?;
        let mut patterns = Vec::new();
        if let Some(root) = &query.root {
            collect_patterns(root, &mut patterns)?;
        }
        let params = query.parameters();
        // A command is written as a `content:` predicate.
        let content: Vec<_> = params.iter().filter(|p| p.field == "content").collect();
        if patterns.len() + content.len() + usize::from(predicate.is_some()) > 1 {
            return Err(ComputeError::Expression);
        }
        let case_sensitive = params
            .iter()
            .rev()
            .find(|p| p.field == "case")
            .is_some_and(|p| p.value.eq_ignore_ascii_case("yes"));
        let last = |field: &str| {
            params
                .iter()
                .rev()
                .find(|p| p.field == field && !p.negated)
                .map(|p| p.value.clone())
        };
        let parameters = params
            .iter()
            .filter(|p| p.field != "content" && p.field != "patterntype")
            .map(|p| Node::Parameter((*p).clone()).to_string())
            .collect();
        let command = if let Some((name, args)) = predicate {
            let (left, right) = split_arrow(&args)?;
            let pattern = if name.ends_with(".structural") {
                sourcegull_structural::Pattern::new(left, None)
                    .map_err(|e| ComputeError::Structural(e.to_string()))?;
                MatchPattern::Structural(left.to_owned())
            } else {
                let site = if name.starts_with("replace") {
                    RegexpSite::Replace
                } else {
                    RegexpSite::Output
                };
                MatchPattern::Regexp {
                    source: left.to_owned(),
                    regex: compile(left, case_sensitive)
                        .map_err(|e| ComputeError::Regexp(site, e))?,
                }
            };
            let template = Template::parse(right);
            if name.starts_with("replace") {
                Command::Replace { pattern, template }
            } else {
                Command::Output {
                    pattern,
                    template,
                    separator: separator.unwrap_or_else(|| "\n".to_owned()),
                    type_value: last("type"),
                    selector: last("select"),
                    kind: match name.as_str() {
                        "output.extra" => OutputKind::Extra,
                        _ => OutputKind::Output,
                    },
                }
            }
        } else {
            let pattern = match (patterns.first(), content.first()) {
                (Some(p), _) => p.clone(),
                (None, Some(c)) if !c.negated => c.value.clone(),
                (None, Some(_)) => return Err(ComputeError::Negated),
                (None, None) => return Err(ComputeError::EmptyPattern),
            };
            let regex = compile(&pattern, case_sensitive)
                .map_err(|e| ComputeError::Regexp(RegexpSite::Pattern, e))?;
            Command::MatchOnly {
                source: pattern,
                regex,
            }
        };
        Ok(ComputeQuery {
            command,
            parameters,
        })
    }

    /// The search query to run and its pattern type, `regexp` or `structural`.
    pub fn search_query(&self) -> (String, PatternType) {
        let (pattern, ty) = match &self.command {
            Command::MatchOnly { source, .. } => (escape_regexp(source), PatternType::Regexp),
            Command::Replace { pattern, .. } | Command::Output { pattern, .. } => match pattern {
                MatchPattern::Regexp { source, .. } => (escape_regexp(source), PatternType::Regexp),
                MatchPattern::Structural(t) => (t.clone(), PatternType::Structural),
            },
        };
        let name = match ty {
            PatternType::Structural => "structural",
            _ => "regexp",
        };
        let mut parts = self.parameters.clone();
        parts.push(format!("patterntype:{name}"));
        parts.push(pattern);
        (parts.join(" "), ty)
    }
}

/// Compiles a pattern the way the search runs it: case-insensitive unless
/// `case:yes`, for every command, so a command matches what the search matched.
fn compile(pattern: &str, case_sensitive: bool) -> Result<Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| e.to_string())
}

fn collect_patterns(node: &Node, out: &mut Vec<String>) -> Result<(), ComputeError> {
    match node {
        Node::Parameter(_) => Ok(()),
        Node::Pattern(p) if p.negated => Err(ComputeError::Negated),
        Node::Pattern(p) => {
            out.push(p.value.clone());
            Ok(())
        }
        Node::Operator { kind, operands, .. } => {
            if *kind == OperatorKind::Or {
                return Err(ComputeError::Expression);
            }
            operands.iter().try_for_each(|o| collect_patterns(o, out))
        }
    }
}

/// Removes a `separator:value` term (value bare or quoted, with `\n`, `\t`
/// escapes) from the query.
fn take_separator(input: &str) -> (String, Option<String>) {
    let mut at = 0;
    while let Some(k) = input[at..].find("separator:") {
        let start = at + k;
        at = start + 1;
        if start > 0 && !input[..start].ends_with(char::is_whitespace) {
            continue;
        }
        let value_start = start + "separator:".len();
        let rest = &input[value_start..];
        let (raw, len) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let mut end = None;
                let mut escaped = false;
                for (i, c) in rest.char_indices().skip(1) {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' => escaped = true,
                        c if c == q => {
                            end = Some(i);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(end) = end else { continue };
                (&rest[1..end], end + 1)
            }
            _ => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], end)
            }
        };
        let value = unescape(raw);
        let mut out = input[..start].to_owned();
        out.push_str(&input[value_start + len..]);
        return (out, Some(value));
    }
    (input.to_owned(), None)
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(e) => out.push(e),
            None => out.push('\\'),
        }
    }
    out
}

/// Removes the `content:<command>(…)` predicate from the query: returns the
/// rest and the command name and arguments. The arguments end at the
/// parenthesis that balances the opening one; backslash escapes and
/// character classes do not count.
fn take_command(input: &str) -> (String, Option<(String, String)>) {
    let mut at = 0;
    while let Some(k) = input[at..].find("content:") {
        let start = at + k;
        at = start + 1;
        if start > 0 && !input[..start].ends_with(|c: char| c.is_whitespace() || c == '(') {
            continue;
        }
        let rest = &input[start + "content:".len()..];
        let Some(open) = rest.find('(') else { continue };
        let name = &rest[..open];
        if !COMMANDS.contains(&name) {
            continue;
        }
        let Some(close) = balanced_end(&rest[open..]) else {
            continue;
        };
        let args = rest[open + 1..open + close].to_owned();
        let end = start + "content:".len() + open + close + 1;
        let mut out = input[..start].to_owned();
        out.push(' ');
        out.push_str(&input[end..]);
        return (out, Some((name.to_owned(), args)));
    }
    (input.to_owned(), None)
}

/// The index of the `)` closing the `(` at the start of `s`.
fn balanced_end(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut class = false;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '[' if !class => class = true,
            ']' if class => class = false,
            '(' if !class => depth += 1,
            ')' if !class => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits a command's arguments at the first `->` into the pattern and the
/// template, trimming the whitespace that touches the arrow.
fn split_arrow(args: &str) -> Result<(&str, &str), ComputeError> {
    let (left, right) = args.split_once("->").ok_or(ComputeError::Arrow)?;
    Ok((left.trim_end(), right.trim_start()))
}

/// Writes a regexp as one pattern of a `patterntype:regexp` query: the
/// characters the query scanner treats specially (whitespace, quotes, a
/// leading `/`, `:` after a field name, parentheses in character classes
/// and escaped parentheses) become `\x` escapes, which mean the same in
/// the regexp.
pub fn escape_regexp(re: &str) -> String {
    let hex = |c: char, out: &mut String| out.push_str(&format!("\\x{{{:X}}}", c as u32));
    let mut out = String::new();
    let mut class = false;
    let mut chars = re.chars().peekable();
    let mut first = true;
    while let Some(c) = chars.next() {
        let at_start = std::mem::take(&mut first);
        match c {
            '\\' => match chars.next() {
                Some(e) if e.is_whitespace() || matches!(e, '(' | ')' | '"' | '\'' | ':') => {
                    hex(e, &mut out)
                }
                Some(e) => {
                    out.push('\\');
                    out.push(e);
                }
                None => out.push('\\'),
            },
            c if c.is_whitespace() => hex(c, &mut out),
            '"' | '\'' => hex(c, &mut out),
            '/' if at_start => hex(c, &mut out),
            '[' if !class => {
                class = true;
                out.push(c);
                // A `]` (after an optional `^`) right at the start is literal.
                if chars.peek() == Some(&'^') {
                    out.push('^');
                    chars.next();
                }
                if chars.peek() == Some(&']') {
                    out.push(']');
                    chars.next();
                }
            }
            ']' if class => {
                class = false;
                out.push(c);
            }
            '(' | ')' if class => hex(c, &mut out),
            // `(?i:` and `(?:` keep their colon.
            ':' if !class && !flag_group(&out) => hex(c, &mut out),
            c => out.push(c),
        }
    }
    // A pattern that is just an operator keyword would read as the operator.
    if ["and", "or", "not"]
        .iter()
        .any(|k| out.eq_ignore_ascii_case(k))
    {
        out = format!("(?:{out})");
    }
    out
}

/// Whether `s` ends with an open flag group (`(?`, `(?i`, `(?-s`, …).
fn flag_group(s: &str) -> bool {
    let trimmed = s.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '-');
    trimmed.ends_with("(?") && !trimmed.ends_with("\\(?")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(q: &str) -> ComputeQuery {
        ComputeQuery::parse(q).unwrap()
    }

    #[test]
    fn match_only() {
        let q = parse("repo:foo (\\w+)Reader case:yes");
        let Command::MatchOnly { source, regex } = &q.command else {
            panic!("{q:?}")
        };
        assert_eq!(source, "(\\w+)Reader");
        assert!(!regex.is_match("bufioreader"));
        assert_eq!(q.parameters, ["repo:foo", "case:yes"]);
        assert_eq!(
            q.search_query().0,
            "repo:foo case:yes patterntype:regexp (\\w+)Reader"
        );
        // Case-insensitive by default.
        let Command::MatchOnly { regex, .. } = parse("foo").command else {
            panic!()
        };
        assert!(regex.is_match("FOO"));
    }

    #[test]
    fn replace_and_output_commands() {
        let q = parse("content:replace(a(b) -> $1x) file:\\.go$");
        let Command::Replace { pattern, template } = &q.command else {
            panic!()
        };
        assert!(matches!(pattern, MatchPattern::Regexp { source, .. } if source == "a(b)"));
        assert_eq!(template, &Template::parse("$1x"));
        assert_eq!(q.parameters, ["file:\\.go$"]);

        let q = parse("repo:x content:output((\\d+) items? -> $1 :: $repo) type:commit");
        let Command::Output {
            pattern,
            separator,
            type_value,
            kind,
            ..
        } = &q.command
        else {
            panic!()
        };
        assert!(
            matches!(pattern, MatchPattern::Regexp { source, .. } if source == "(\\d+) items?")
        );
        assert_eq!(separator, "\n");
        assert_eq!(type_value.as_deref(), Some("commit"));
        assert_eq!(*kind, OutputKind::Output);

        let q = parse("content:output.extra(TODO\\((\\w+)\\) -> $1) separator:\", \"");
        let Command::Output {
            separator, kind, ..
        } = &q.command
        else {
            panic!()
        };
        assert_eq!(separator, ", ");
        assert_eq!(*kind, OutputKind::Extra);
        assert!(q.parameters.is_empty());

        let q = parse("content:output.structural(fmt.Println(:[x]) -> :[x]) lang:go");
        let Command::Output { pattern, .. } = &q.command else {
            panic!()
        };
        assert_eq!(
            pattern,
            &MatchPattern::Structural("fmt.Println(:[x])".into())
        );
        assert_eq!(
            q.search_query(),
            (
                "lang:go patterntype:structural fmt.Println(:[x])".to_owned(),
                PatternType::Structural
            )
        );
    }

    #[test]
    fn errors() {
        let err = |q: &str| ComputeQuery::parse(q).unwrap_err();
        assert_eq!(err("repo:x"), ComputeError::EmptyPattern);
        assert_eq!(err("a or b"), ComputeError::Expression);
        assert_eq!(err("a content:b"), ComputeError::Expression);
        assert_eq!(err("content:output(foo)"), ComputeError::Arrow);
        assert!(matches!(
            err("content:replace(a{2,1} -> x)"),
            ComputeError::Regexp(..)
        ));
        assert!(matches!(err("-content:foo"), ComputeError::Negated));
    }

    #[test]
    fn arrow_split() {
        assert_eq!(split_arrow("x  ->\t$1"), Ok(("x", "$1")));
        assert_eq!(split_arrow("a->b->c"), Ok(("a", "b->c")));
        assert_eq!(split_arrow("p ->"), Ok(("p", "")));
        assert_eq!(split_arrow(" lead -> trail "), Ok((" lead", "trail ")));
        assert_eq!(split_arrow("a -> -> b"), Ok(("a", "-> b")));
        assert_eq!(split_arrow("a - > b"), Err(ComputeError::Arrow));
        assert_eq!(split_arrow("no arrow"), Err(ComputeError::Arrow));
    }

    #[test]
    fn error_sites_and_wording() {
        let err = |q: &str| ComputeQuery::parse(q).unwrap_err();
        let empty = err("repo:a");
        assert_eq!(empty, ComputeError::EmptyPattern);
        assert_eq!(
            empty.to_string(),
            "a compute query needs a search pattern or a content: command"
        );
        assert_eq!(err("-content:zzz"), ComputeError::Negated);
        assert!(ComputeQuery::parse("content:replace(q(r) -> $1)").is_ok());
        assert_eq!(err("content:output(qq)"), ComputeError::Arrow);
        assert!(matches!(
            err("content:replace(b{3,2} -> x)"),
            ComputeError::Regexp(RegexpSite::Replace, _)
        ));
        assert!(matches!(
            err("content:output(c{3,2} -> x)"),
            ComputeError::Regexp(RegexpSite::Output, _)
        ));
        let bad = err("d{3,2}");
        assert!(matches!(bad, ComputeError::Regexp(RegexpSite::Pattern, _)));
        assert!(
            bad.to_string()
                .starts_with("invalid regexp in the search pattern: ")
        );
    }

    /// The escaped pattern parses back as one pattern that means the same regexp.
    #[test]
    fn escaped_patterns_round_trip() {
        for re in [
            "foo bar",
            "\"quoted\"",
            "/slash/",
            "path:(\\w+)",
            "(?i:x)\\(y",
            "[()]+ end",
            "[^]x]",
            "[[:alpha:]]+",
            "a\\ b\\)",
            "or",
        ] {
            let escaped = escape_regexp(re);
            let q = sourcegull_query::parse(&escaped, PatternType::Regexp).unwrap();
            let Some(Node::Pattern(p)) = &q.root else {
                panic!("{re:?} → {escaped:?} → {q:?}")
            };
            assert_eq!(p.value, escaped, "{re:?}");
            let a = Regex::new(re).unwrap();
            let b = Regex::new(&escaped).unwrap();
            for s in [
                "foo bar",
                "\"quoted\"",
                "/slash/",
                "path:abc",
                "X(y",
                "() end",
                "q",
                "abc",
                "a b)",
                "or",
            ] {
                assert_eq!(
                    a.find(s).map(|m| m.range()),
                    b.find(s).map(|m| m.range()),
                    "{re:?} on {s:?}"
                );
            }
        }
    }
}
