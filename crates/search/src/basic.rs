//! Basic queries: parameters as a flat list, patterns as a tree.
//!
//! OR groups that contain parameters are distributed, so
//! `(repo:a foo) or (repo:b bar)` becomes two basic queries that are planned
//! and run separately. OR groups of patterns only stay in the pattern tree,
//! where the backends evaluate them.

use sourcegull_query::{Node, OperatorKind, Parameter, Query};

/// Upper bound on the basic queries one query expands to.
pub const MAX_BASIC_QUERIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicQuery {
    pub parameters: Vec<Parameter>,
    pub pattern: Option<Node>,
}

impl BasicQuery {
    /// Parameters of `field` (canonical name).
    pub fn fields<'a>(&'a self, field: &'a str) -> impl Iterator<Item = &'a Parameter> + 'a {
        self.parameters.iter().filter(move |p| p.field == field)
    }

    /// Value of the last non-negated parameter of `field`.
    pub fn value<'a>(&'a self, field: &'a str) -> Option<&'a str> {
        self.fields(field)
            .filter(|p| !p.negated)
            .last()
            .map(|p| p.value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "query expands to more than {MAX_BASIC_QUERIES} separate searches; simplify the OR groups that contain filters"
)]
pub struct TooManyBasicQueries;

/// Expands a query into basic queries. An empty query gives none.
pub fn basic_queries(query: &Query) -> Result<Vec<BasicQuery>, TooManyBasicQueries> {
    match &query.root {
        None => Ok(Vec::new()),
        Some(root) => expand(root),
    }
}

fn expand(node: &Node) -> Result<Vec<BasicQuery>, TooManyBasicQueries> {
    Ok(match node {
        Node::Parameter(p) => vec![BasicQuery {
            parameters: vec![p.clone()],
            pattern: None,
        }],
        Node::Pattern(_) => vec![BasicQuery {
            parameters: Vec::new(),
            pattern: Some(node.clone()),
        }],
        Node::Operator {
            kind: OperatorKind::Or,
            operands,
            ..
        } if !operands.iter().any(has_parameter) => {
            vec![BasicQuery {
                parameters: Vec::new(),
                pattern: Some(node.clone()),
            }]
        }
        Node::Operator {
            kind: OperatorKind::Or,
            operands,
            ..
        } => {
            let mut out = Vec::new();
            for operand in operands {
                out.extend(expand(operand)?);
                if out.len() > MAX_BASIC_QUERIES {
                    return Err(TooManyBasicQueries);
                }
            }
            out
        }
        Node::Operator { kind, operands, .. } => {
            let mut acc = vec![BasicQuery {
                parameters: Vec::new(),
                pattern: None,
            }];
            for operand in operands {
                let alternatives = expand(operand)?;
                if acc.len() * alternatives.len() > MAX_BASIC_QUERIES {
                    return Err(TooManyBasicQueries);
                }
                acc = acc
                    .iter()
                    .flat_map(|left| {
                        alternatives
                            .iter()
                            .map(move |right| combine(*kind, left, right))
                    })
                    .collect();
            }
            acc
        }
    })
}

fn has_parameter(node: &Node) -> bool {
    match node {
        Node::Parameter(_) => true,
        Node::Pattern(_) => false,
        Node::Operator { operands, .. } => operands.iter().any(has_parameter),
    }
}

fn combine(kind: OperatorKind, left: &BasicQuery, right: &BasicQuery) -> BasicQuery {
    let mut parameters = left.parameters.clone();
    parameters.extend(right.parameters.iter().cloned());
    let pattern = match (&left.pattern, &right.pattern) {
        (None, p) | (p, None) => p.clone(),
        (Some(a), Some(b)) => {
            let span = sourcegull_query::Span {
                start: a.span().start,
                end: b.span().end,
            };
            let mut operands = Vec::new();
            for node in [a, b] {
                match node {
                    Node::Operator {
                        kind: k,
                        operands: inner,
                        ..
                    } if *k == kind => operands.extend(inner.clone()),
                    node => operands.push(node.clone()),
                }
            }
            Some(Node::Operator {
                kind,
                operands,
                span,
            })
        }
    };
    BasicQuery {
        parameters,
        pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_query::{PatternType, parse};

    fn basics(q: &str) -> Vec<(Vec<String>, String)> {
        basic_queries(&parse(q, PatternType::Keyword).unwrap())
            .unwrap()
            .into_iter()
            .map(|b| {
                let params = b
                    .parameters
                    .iter()
                    .map(|p| format!("{}:{}", p.field, p.value))
                    .collect();
                (params, b.pattern.map(|p| p.to_string()).unwrap_or_default())
            })
            .collect()
    }

    #[test]
    fn single() {
        assert_eq!(
            basics("repo:a foo bar"),
            vec![(vec!["repo:a".into()], "foo bar".into())]
        );
        assert!(basics("").is_empty());
    }

    #[test]
    fn pattern_or_stays_in_tree() {
        assert_eq!(
            basics("repo:a (foo or bar)"),
            vec![(vec!["repo:a".into()], "foo OR bar".into())]
        );
    }

    #[test]
    fn parameter_or_is_distributed() {
        assert_eq!(
            basics("(repo:a or repo:b) foo"),
            vec![
                (vec!["repo:a".into()], "foo".into()),
                (vec!["repo:b".into()], "foo".into()),
            ]
        );
        assert_eq!(
            basics("(repo:a x) or (repo:b y) z"),
            vec![
                (vec!["repo:a".into()], "x".into()),
                (vec!["repo:b".into()], "y z".into()),
            ]
        );
    }

    #[test]
    fn hoisted_fields_scope_the_whole_or() {
        // Without parentheses the parser hoists the fields, so this is
        // one search, not `(repo:a foo) or (bar file:x)`.
        assert_eq!(
            basics("repo:a foo or bar file:x"),
            vec![(vec!["repo:a".into(), "file:x".into()], "foo OR bar".into())]
        );
    }

    #[test]
    fn too_many() {
        let q = (0..6)
            .map(|i| format!("(repo:a{i} or repo:b{i})"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            basic_queries(&parse(&q, PatternType::Keyword).unwrap()),
            Err(TooManyBasicQueries)
        );
    }
}
