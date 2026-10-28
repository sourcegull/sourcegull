//! Query validation.
//!
//! Checks field values, repetition and negation, predicate arguments, and
//! fields that only apply to commit and diff searches. Regexp syntax and
//! date expressions are checked where they are compiled.

use crate::{Parameter, PatternType, PredicateError, Query, Span};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("invalid value {value:?} for {field}: expected {expected}")]
    InvalidValue {
        field: &'static str,
        value: String,
        expected: &'static str,
        span: Span,
    },
    #[error("{field}: may not be used more than once")]
    Repeated { field: &'static str, span: Span },
    #[error("{field}: does not support negation")]
    Negated { field: &'static str, span: Span },
    #[error("{field}: requires type:commit or type:diff")]
    RequiresCommitType { field: &'static str, span: Span },
    #[error("{source}")]
    Predicate { source: PredicateError, span: Span },
    #[error("{field}: is not supported in structural search")]
    Structural { field: &'static str, span: Span },
}

impl ValidationError {
    pub fn span(&self) -> Span {
        match self {
            ValidationError::InvalidValue { span, .. }
            | ValidationError::Repeated { span, .. }
            | ValidationError::Negated { span, .. }
            | ValidationError::RequiresCommitType { span, .. }
            | ValidationError::Predicate { span, .. }
            | ValidationError::Structural { span, .. } => *span,
        }
    }
}

/// Fields that may appear at most once.
const SINGULAR: &[&str] = &[
    "case",
    "fork",
    "archived",
    "visibility",
    "count",
    "timeout",
    "index",
    "patterntype",
    "select",
    "context",
];

/// Fields that accept `-field:`.
const NEGATABLE: &[&str] = &[
    "repo",
    "file",
    "lang",
    "content",
    "author",
    "committer",
    "message",
];

/// Fields that only apply to commit and diff searches.
const COMMIT_ONLY: &[&str] = &["author", "committer", "message", "before", "after"];

const SYMBOL_KINDS: &[&str] = &[
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enum-member",
    "struct",
    "event",
    "operator",
    "type-parameter",
];

/// Validates a parsed query. Returns the first problem, in query order.
pub fn validate(query: &Query) -> Result<(), ValidationError> {
    let params = query.parameters();
    let mut seen: Vec<&str> = Vec::new();
    for p in &params {
        if SINGULAR.contains(&p.field) {
            if seen.contains(&p.field) {
                return Err(ValidationError::Repeated {
                    field: p.field,
                    span: p.span,
                });
            }
            seen.push(p.field);
        }
        if p.negated && !NEGATABLE.contains(&p.field) {
            return Err(ValidationError::Negated {
                field: p.field,
                span: p.span,
            });
        }
        if let Some(Err(source)) = p.predicate() {
            return Err(ValidationError::Predicate {
                source,
                span: p.span,
            });
        }
        validate_value(p)?;
    }

    let commit_type = params
        .iter()
        .any(|p| p.field == "type" && !p.negated && matches!(p.value.as_str(), "commit" | "diff"));
    if !commit_type && let Some(p) = params.iter().find(|p| COMMIT_ONLY.contains(&p.field)) {
        return Err(ValidationError::RequiresCommitType {
            field: p.field,
            span: p.span,
        });
    }
    // Structural search matches file contents in indexed repos only.
    if query.pattern_type == PatternType::Structural
        && let Some(p) = params.iter().find(|p| {
            matches!(p.field, "type" | "case")
                || (p.field == "index" && p.value.eq_ignore_ascii_case("no"))
        })
    {
        return Err(ValidationError::Structural {
            field: p.field,
            span: p.span,
        });
    }
    Ok(())
}

fn validate_value(p: &Parameter) -> Result<(), ValidationError> {
    let v = p.value.as_str();
    let lower = v.to_ascii_lowercase();
    let (ok, expected) = match p.field {
        "case" => (
            matches!(lower.as_str(), "yes" | "no" | "y" | "n" | "true" | "false"),
            "yes or no",
        ),
        "fork" | "archived" | "index" => (yes_no_only(&lower), "yes, no or only"),
        "visibility" => (
            matches!(lower.as_str(), "any" | "public" | "private"),
            "any, public or private",
        ),
        "count" => (
            lower == "all" || v.parse::<u32>().is_ok_and(|n| n > 0),
            "a positive number or all",
        ),
        "timeout" => (is_duration(v), "a duration such as 10s or 1m30s"),
        "type" => (
            matches!(
                lower.as_str(),
                "repo" | "path" | "file" | "symbol" | "commit" | "diff"
            ),
            "repo, path, file, symbol, commit or diff",
        ),
        "patterntype" => (
            PatternType::from_name(v).is_some(),
            "keyword, standard, literal, regexp or structural",
        ),
        "select" => (
            is_select(&lower),
            "a select path such as repo, file or symbol.function",
        ),
        "before" | "after" | "author" | "committer" | "context" => (!v.is_empty(), "a value"),
        _ => (true, ""),
    };
    if ok {
        return Ok(());
    }
    Err(ValidationError::InvalidValue {
        field: p.field,
        value: v.to_string(),
        expected,
        span: p.span,
    })
}

fn yes_no_only(v: &str) -> bool {
    matches!(v, "yes" | "no" | "only" | "y" | "n" | "true" | "false")
}

/// Go-style duration: one or more `<number><unit>` with units ns, us, ms,
/// s, m, h (`10s`, `1m30s`, `1.5h`).
fn is_duration(s: &str) -> bool {
    let mut rest = s;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let num = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num == 0 || rest[..num].parse::<f64>().is_err() {
            return false;
        }
        rest = &rest[num..];
        let Some(unit) = ["ns", "us", "µs", "ms", "s", "m", "h"]
            .iter()
            .find(|u| rest.starts_with(*u))
        else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    true
}

fn is_select(v: &str) -> bool {
    match v.split_once('.') {
        None => matches!(v, "repo" | "file" | "content" | "symbol" | "commit"),
        Some(("file", sub)) => matches!(sub, "directory" | "path" | "owners"),
        Some(("symbol", kind)) => SYMBOL_KINDS.contains(&kind),
        Some(("commit", sub)) => matches!(sub, "diff.added" | "diff.removed"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn check(q: &str) -> Result<(), ValidationError> {
        validate(&parse(q, PatternType::Keyword).unwrap())
    }

    #[test]
    fn valid_queries() {
        for q in [
            "foo repo:x -repo:y file:z lang:go count:all case:yes fork:only archived:no",
            "type:commit author:alice after:\"1 week ago\" -message:wip",
            "timeout:1m30s select:symbol.function visibility:private index:only",
            "repo:has.file(path:go.mod) select:repo",
            "type:diff select:commit.diff.added foo",
        ] {
            assert_eq!(check(q), Ok(()), "{q}");
        }
    }

    #[test]
    fn invalid_values() {
        for q in [
            "case:maybe",
            "fork:sometimes",
            "count:0",
            "count:many",
            "timeout:10",
            "timeout:s",
            "type:issue",
            "select:symbol.widget",
            "select:file.size",
            "patterntype:fuzzy",
            "visibility:internal",
        ] {
            assert!(
                matches!(check(q), Err(ValidationError::InvalidValue { .. })),
                "{q}"
            );
        }
    }

    #[test]
    fn repetition_and_negation() {
        assert!(matches!(
            check("case:yes case:no"),
            Err(ValidationError::Repeated { field: "case", .. })
        ));
        assert!(matches!(
            check("-fork:yes"),
            Err(ValidationError::Negated { field: "fork", .. })
        ));
        assert!(matches!(
            check("NOT count:all"),
            Err(ValidationError::Negated { .. })
        ));
        assert_eq!(check("repo:a repo:b"), Ok(()));
    }

    #[test]
    fn commit_fields() {
        let err = check("author:alice foo").unwrap_err();
        assert!(matches!(
            err,
            ValidationError::RequiresCommitType {
                field: "author",
                ..
            }
        ));
        assert_eq!(err.span(), Span { start: 0, end: 12 });
        assert!(check("-type:commit before:yesterday").is_err());
    }

    #[test]
    fn predicates() {
        assert!(matches!(
            check("repo:has.file(lang:go)"),
            Err(ValidationError::Predicate { .. })
        ));
        assert_eq!(check("file:foo(bar)"), Ok(()));
    }

    #[test]
    fn structural() {
        let q = |s| validate(&parse(s, PatternType::Structural).unwrap());
        assert_eq!(q("foo(:[args]) lang:go"), Ok(()));
        assert!(matches!(
            q("foo(:[args]) type:symbol"),
            Err(ValidationError::Structural { .. })
        ));
        assert!(matches!(
            q("foo(:[args]) index:no"),
            Err(ValidationError::Structural { .. })
        ));
    }
}
