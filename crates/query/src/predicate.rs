//! Field predicates such as `repo:has.file(path:go.mod)`.
//!
//! A value that only looks like a call, with a name that is not a predicate of
//! its field (`file:foo(bar)`), is a regular value.

use crate::{Node, ParseError, PatternType, parse};

/// A key or value in `repo:has.meta(...)`: `/.../` is a regexp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaMatch {
    Exact(String),
    Regexp(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// `repo:has.file(path:… content:…)`: at least one of the two is set.
    RepoHasFile {
        path: Option<String>,
        content: Option<String>,
    },
    RepoHasPath(String),
    RepoHasContent(String),
    /// Date expression, parsed later (`1 month ago`).
    RepoHasCommitAfter(String),
    RepoHasDescription(String),
    RepoHasTopic(String),
    /// `has.meta(key)` matches any value; `has.meta(key:value)` a value,
    /// where an empty value matches a key with no value.
    RepoHasMeta {
        key: MetaMatch,
        value: Option<MetaMatch>,
    },
    FileHasContent(String),
    /// `file:has.owner()` without an argument matches files with any owner.
    FileHasOwner(Option<String>),
    FileHasContributor(String),
    /// `rev:at.time(time[, ref])`.
    RevAtTime {
        time: String,
        rev: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PredicateError {
    #[error("{name} requires an argument")]
    MissingArgument { name: String },
    #[error("{name} does not take an argument")]
    UnexpectedArgument { name: String },
    #[error("{name}: invalid argument {arg:?}: {reason}")]
    InvalidArgument {
        name: String,
        arg: String,
        reason: &'static str,
    },
}

/// Predicate names per field (canonical names), as [`parse_predicate`] accepts them.
const PREDICATES: &[(&str, &[&str])] = &[
    (
        "repo",
        &[
            "has.file",
            "contains.file",
            "has.path",
            "contains.path",
            "has.content",
            "contains.content",
            "has.commit.after",
            "contains.commit.after",
            "has.description",
            "has.topic",
            "has.meta",
            "has.key",
            "has.tag",
            "has",
        ],
    ),
    (
        "file",
        &[
            "has.content",
            "contains.content",
            "has.owner",
            "has.contributor",
        ],
    ),
    ("rev", &["at.time"]),
];

/// Whether `name` (any case) is a predicate of `field` (canonical name).
pub(crate) fn is_predicate(field: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    PREDICATES
        .iter()
        .any(|(f, names)| *f == field && names.contains(&name.as_str()))
}

/// Parses `value` as a predicate of `field` (canonical name). `None` means
/// the value is not a predicate call.
pub fn parse_predicate(field: &str, value: &str) -> Option<Result<Predicate, PredicateError>> {
    let open = value.find('(')?;
    let args = value.strip_suffix(')')?.get(open + 1..)?;
    let name = value[..open].to_ascii_lowercase();
    if !crate::parser::balanced(&value[open..]) {
        return None;
    }
    let qualified = format!("{field}:{name}");
    let arg = || -> Result<String, PredicateError> {
        let arg = args.trim();
        if arg.is_empty() {
            return Err(PredicateError::MissingArgument {
                name: qualified.clone(),
            });
        }
        Ok(arg.to_string())
    };
    let pred = match (field, name.as_str()) {
        ("repo", "has.file" | "contains.file") => repo_has_file(&qualified, args),
        ("repo", "has.path" | "contains.path") => arg().map(Predicate::RepoHasPath),
        ("repo", "has.content" | "contains.content") => arg().map(Predicate::RepoHasContent),
        ("repo", "has.commit.after" | "contains.commit.after") => {
            arg().map(Predicate::RepoHasCommitAfter)
        }
        ("repo", "has.description") => arg().map(Predicate::RepoHasDescription),
        ("repo", "has.topic") => arg().map(Predicate::RepoHasTopic),
        ("repo", "has.meta" | "has.key" | "has.tag") => arg().map(|a| repo_has_meta(&name, &a)),
        ("repo", "has") => arg().map(|a| repo_has_meta("has.meta", &a)),
        ("file", "has.content" | "contains.content") => arg().map(Predicate::FileHasContent),
        ("file", "has.owner") => Ok(Predicate::FileHasOwner(arg().ok())),
        ("file", "has.contributor") => arg().map(Predicate::FileHasContributor),
        ("rev", "at.time") => arg().and_then(|a| {
            let mut parts = a.splitn(2, ',').map(str::trim);
            let time = parts.next().unwrap_or_default().to_string();
            let rev = parts.next().map(str::to_string);
            if time.is_empty() || rev.as_deref() == Some("") {
                return Err(PredicateError::InvalidArgument {
                    name: qualified.clone(),
                    arg: a.clone(),
                    reason: "expected time[, rev]",
                });
            }
            Ok(Predicate::RevAtTime { time, rev })
        }),
        _ => return None,
    };
    Some(pred)
}

fn repo_has_file(name: &str, args: &str) -> Result<Predicate, PredicateError> {
    let invalid = |arg: &str, reason| PredicateError::InvalidArgument {
        name: name.into(),
        arg: arg.into(),
        reason,
    };
    let query = parse(args, PatternType::Literal)
        .map_err(|e: ParseError| invalid(args, parse_reason(&e)))?;
    let (mut path, mut content) = (None, None);
    let mut nodes = vec![];
    match query.root {
        None => return Err(PredicateError::MissingArgument { name: name.into() }),
        Some(Node::Operator {
            kind: crate::OperatorKind::Concat,
            operands,
            ..
        }) => nodes.extend(operands),
        Some(node) => nodes.push(node),
    }
    for node in nodes {
        let Node::Parameter(p) = node else {
            return Err(invalid(
                &args[node.span().start..node.span().end],
                "expected path: or content:",
            ));
        };
        if p.negated {
            return Err(invalid(
                &args[p.span.start..p.span.end],
                "negation is not supported",
            ));
        }
        let slot = match p.field {
            "file" => &mut path,
            "content" => &mut content,
            _ => {
                return Err(invalid(
                    &args[p.span.start..p.span.end],
                    "expected path: or content:",
                ));
            }
        };
        if slot.replace(p.value).is_some() {
            return Err(invalid(
                &args[p.span.start..p.span.end],
                "repeated argument",
            ));
        }
    }
    Ok(Predicate::RepoHasFile { path, content })
}

fn parse_reason(e: &ParseError) -> &'static str {
    match e {
        ParseError::UnbalancedParens(_) | ParseError::UnsupportedParens { .. } => {
            "unbalanced parentheses"
        }
        ParseError::UnterminatedQuote(_) => "unterminated quoted string",
        ParseError::MissingOperand { .. }
        | ParseError::NegatedExpression(_)
        | ParseError::NegatedParameter { .. }
        | ParseError::UnclearParens(_) => "expected path: or content:",
    }
}

fn repo_has_meta(name: &str, arg: &str) -> Predicate {
    let meta = |s: &str| match s.strip_prefix('/').and_then(|s| s.strip_suffix('/')) {
        Some(re) if !re.is_empty() => MetaMatch::Regexp(re.to_string()),
        _ => MetaMatch::Exact(s.to_string()),
    };
    // Legacy `has(key:value)` and `has.key(key)`; `has.tag(t)` is a key
    // with no value.
    match (name, split_meta(arg)) {
        ("has.tag", _) => Predicate::RepoHasMeta {
            key: meta(arg),
            value: Some(MetaMatch::Exact(String::new())),
        },
        ("has.key", _) | (_, None) => Predicate::RepoHasMeta {
            key: meta(arg),
            value: None,
        },
        (_, Some((k, v))) => Predicate::RepoHasMeta {
            key: meta(k),
            value: Some(meta(v)),
        },
    }
}

/// Splits `key:value` at the first `:` outside a `/regexp/` key.
fn split_meta(arg: &str) -> Option<(&str, &str)> {
    if let Some(rest) = arg.strip_prefix('/')
        && let Some(end) = rest.find("/:")
    {
        return Some((&arg[..end + 2], &arg[end + 3..]));
    }
    arg.split_once(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(field: &str, value: &str) -> Predicate {
        parse_predicate(field, value)
            .expect("predicate")
            .expect("valid")
    }

    #[test]
    fn not_a_predicate() {
        assert_eq!(parse_predicate("file", "foo(bar)"), None);
        assert_eq!(parse_predicate("repo", "^gitlab"), None);
        assert_eq!(parse_predicate("lang", "has.file(x)"), None);
        assert_eq!(parse_predicate("repo", "has.path(a"), None);
    }

    #[test]
    fn repo_has_file() {
        assert_eq!(
            p("repo", "has.file(path:go.mod content:\"golang.org/x\")"),
            Predicate::RepoHasFile {
                path: Some("go.mod".into()),
                content: Some("golang.org/x".into())
            }
        );
        assert_eq!(
            p("repo", "contains.file(content:TODO)"),
            Predicate::RepoHasFile {
                path: None,
                content: Some("TODO".into())
            }
        );
        for bad in [
            "has.file()",
            "has.file(foo)",
            "has.file(path:a path:b)",
            "has.file(-path:a)",
            "has.file(lang:go)",
        ] {
            assert!(parse_predicate("repo", bad).unwrap().is_err(), "{bad}");
        }
    }

    #[test]
    fn repo_has_meta() {
        assert_eq!(
            p("repo", "has.meta(team:search)"),
            Predicate::RepoHasMeta {
                key: MetaMatch::Exact("team".into()),
                value: Some(MetaMatch::Exact("search".into()))
            }
        );
        assert_eq!(
            p("repo", "has.meta(/te.m/:/s.*/)"),
            Predicate::RepoHasMeta {
                key: MetaMatch::Regexp("te.m".into()),
                value: Some(MetaMatch::Regexp("s.*".into()))
            }
        );
        assert_eq!(
            p("repo", "has.meta(team)"),
            Predicate::RepoHasMeta {
                key: MetaMatch::Exact("team".into()),
                value: None
            }
        );
        assert_eq!(
            p("repo", "has.tag(go)"),
            Predicate::RepoHasMeta {
                key: MetaMatch::Exact("go".into()),
                value: Some(MetaMatch::Exact(String::new()))
            }
        );
    }

    #[test]
    fn others() {
        assert_eq!(
            p("repo", "has.commit.after(1 month ago)"),
            Predicate::RepoHasCommitAfter("1 month ago".into())
        );
        assert_eq!(
            p("repo", "HAS.TOPIC(rust)"),
            Predicate::RepoHasTopic("rust".into())
        );
        assert_eq!(p("file", "has.owner()"), Predicate::FileHasOwner(None));
        assert_eq!(
            p("file", "has.owner(@alice)"),
            Predicate::FileHasOwner(Some("@alice".into()))
        );
        assert_eq!(
            p("rev", "at.time(2024-01-01, main)"),
            Predicate::RevAtTime {
                time: "2024-01-01".into(),
                rev: Some("main".into())
            }
        );
        assert!(parse_predicate("repo", "has.path()").unwrap().is_err());
        assert!(parse_predicate("rev", "at.time(,main)").unwrap().is_err());
    }
}
