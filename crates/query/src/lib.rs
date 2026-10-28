//! sourcegull-query: the search query language.
//!
//! This crate holds the lexer, the parser, the AST and the printer. The AST
//! carries byte spans so the UI can highlight queries, and the crate has no
//! runtime dependencies so it can be compiled to WASM for the query box.
//!
//! Grammar:
//!
//! ```text
//! query   := or
//! or      := and ("OR" and)*
//! and     := concat ("AND" concat)*
//! concat  := unary+                     (implicit operator between terms)
//! unary   := "NOT" atom | atom
//! atom    := "(" or ")" | ["-"]field ":" value | pattern
//! ```
//!
//! `AND`, `OR` and `NOT` match case-insensitively, and only between terms:
//! `and`/`or` follow whitespace or `)` and precede whitespace or `(`, `not`
//! precedes whitespace (`and b` and `a or` are patterns). Field names have
//! aliases (`r:` is `repo:`); unknown `foo:bar` is a pattern. A query whose
//! `(` is never closed, or that misses an operand, is parsed again with
//! parentheses as literal characters (`if (x`); a `)` that closes no group is
//! an error (`x) or y`). Outside keyword queries, balanced parentheses holding
//! no field or operator are pattern text too (`copyright (c) 2005`), not
//! grouping, and a grouped `and`/`or` among other terms (`a (b or c) d`) is an
//! error in standard and literal queries. Keyword, standard and regexp queries
//! read `/.../` as a regexp; keyword and regexp queries read `"..."` and
//! `'...'` as quoted literals.
//! Validation and planning come later in the pipeline.

mod parser;
mod predicate;
mod validate;

use std::fmt;

pub use parser::parse;
pub use predicate::{MetaMatch, Predicate, PredicateError, parse_predicate};
pub use validate::{ValidationError, validate};

/// A byte range in the query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// How patterns are interpreted (`patterntype:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternType {
    /// Terms ANDed; quoted phrases match exactly.
    #[default]
    Keyword,
    /// Literal, plus `/regex/`.
    Standard,
    Literal,
    Regexp,
    Structural,
}

impl PatternType {
    pub fn from_name(name: &str) -> Option<PatternType> {
        Some(match name.to_ascii_lowercase().as_str() {
            "keyword" => PatternType::Keyword,
            "standard" => PatternType::Standard,
            "literal" => PatternType::Literal,
            "regexp" | "regex" => PatternType::Regexp,
            "structural" => PatternType::Structural,
            _ => return None,
        })
    }
}

/// Recognized fields with their aliases. The first entry is canonical.
pub const FIELDS: &[&[&str]] = &[
    &["repo", "r"],
    &["file", "f", "path"],
    &["lang", "l", "language"],
    &["rev", "revision"],
    &["context"],
    &["case"],
    &["fork"],
    &["archived"],
    &["visibility"],
    &["count"],
    &["timeout"],
    &["index"],
    &["select"],
    &["type"],
    &["author"],
    &["committer"],
    &["message", "m", "msg"],
    &["before", "until"],
    &["after", "since"],
    &["rule"],
    &["content"],
    &["patterntype"],
];

/// Canonical name of a field or alias (case-insensitive).
pub fn canonical_field(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    FIELDS
        .iter()
        .find(|names| names.contains(&lower.as_str()))
        .map(|names| names[0])
}

/// `field:value`, possibly negated (`-field:` or `NOT field:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// Canonical field name.
    pub field: &'static str,
    pub value: String,
    pub negated: bool,
    /// The value was written in quotes.
    pub quoted: bool,
    pub span: Span,
}

impl Parameter {
    /// The predicate this parameter calls, if its value is one
    /// (`repo:has.file(...)`).
    pub fn predicate(&self) -> Option<Result<Predicate, PredicateError>> {
        if self.quoted {
            None
        } else {
            parse_predicate(self.field, &self.value)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// Interpreted per the query's pattern type.
    Literal,
    /// `"..."` or `'...'` in keyword and regexp queries: matched literally.
    Quoted,
    /// `/.../` in keyword, standard and regexp queries.
    Regexp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub value: String,
    pub kind: PatternKind,
    pub negated: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorKind {
    And,
    Or,
    /// Terms written next to each other. Keyword queries treat it as AND;
    /// literal queries join the patterns with spaces.
    Concat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Parameter(Parameter),
    Pattern(Pattern),
    Operator {
        kind: OperatorKind,
        operands: Vec<Node>,
        span: Span,
    },
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Node::Parameter(p) => p.span,
            Node::Pattern(p) => p.span,
            Node::Operator { span, .. } => *span,
        }
    }
}

/// A parsed query. An empty query has no root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub pattern_type: PatternType,
    pub root: Option<Node>,
}

impl Query {
    /// All parameters in the query, depth first.
    pub fn parameters(&self) -> Vec<&Parameter> {
        fn walk<'a>(node: &'a Node, out: &mut Vec<&'a Parameter>) {
            match node {
                Node::Parameter(p) => out.push(p),
                Node::Pattern(_) => {}
                Node::Operator { operands, .. } => operands.iter().for_each(|n| walk(n, out)),
            }
        }
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            walk(root, &mut out);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unbalanced parentheses at {}", .0.start)]
    UnbalancedParens(Span),
    #[error("unterminated quoted string at {}", .0.start)]
    UnterminatedQuote(Span),
    #[error("{op} at {} has no operand on the {side}", .span.start)]
    MissingOperand {
        op: &'static str,
        side: &'static str,
        span: Span,
    },
    #[error("NOT at {} must be followed by a pattern or field, not an expression", .0.start)]
    NegatedExpression(Span),
    /// `NOT -field:value`. `text` is the parameter as written.
    #[error("unexpected NOT before \"{text}\". Remove NOT and try again")]
    NegatedParameter { text: String, span: Span },
    /// A parenthesized AND/OR among the terms of a standard or literal
    /// query (`a (b or c) d`).
    #[error(
        "I'm having trouble understanding that query. The combination of parentheses is the problem. Try using the content: filter to quote patterns that contain parentheses"
    )]
    UnclearParens(Span),
    /// A `)` that closes no group (`x) or y`). `quotes` picks the keyword
    /// query's hint.
    #[error("{}", unsupported_parens(*.quotes))]
    UnsupportedParens { span: Span, quotes: bool },
}

fn unsupported_parens(quotes: bool) -> &'static str {
    if quotes {
        "unsupported expression. These parentheses do not form clear groups. Put a pattern that contains parentheses in double quotes (\"...\")"
    } else {
        "unsupported expression. The combination of parentheses in the query have an unclear meaning. Try using the content: filter to quote patterns that contain parentheses"
    }
}

impl ParseError {
    pub fn span(&self) -> Span {
        match self {
            ParseError::UnbalancedParens(span)
            | ParseError::UnterminatedQuote(span)
            | ParseError::NegatedExpression(span)
            | ParseError::UnclearParens(span)
            | ParseError::UnsupportedParens { span, .. }
            | ParseError::MissingOperand { span, .. }
            | ParseError::NegatedParameter { span, .. } => *span,
        }
    }
}

/// Canonical form: `parse(q.to_string())` gives the same tree (spans aside).
impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.root {
            Some(root) => write!(f, "{root}"),
            None => Ok(()),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Parameter(p) => {
                if p.negated {
                    f.write_str("-")?;
                }
                write!(f, "{}:", p.field)?;
                if p.quoted || needs_quotes(&p.value) {
                    write_quoted(f, &p.value)
                } else {
                    f.write_str(&p.value)
                }
            }
            Node::Pattern(p) => {
                if p.negated {
                    f.write_str("NOT ")?;
                }
                match p.kind {
                    PatternKind::Literal => f.write_str(&p.value),
                    PatternKind::Quoted => write_delimited(f, &p.value, '"'),
                    PatternKind::Regexp => write_delimited(f, &p.value, '/'),
                }
            }
            Node::Operator { kind, operands, .. } => {
                let sep = match kind {
                    OperatorKind::And => " AND ",
                    OperatorKind::Or => " OR ",
                    OperatorKind::Concat => " ",
                };
                for (i, operand) in operands.iter().enumerate() {
                    if i > 0 {
                        f.write_str(sep)?;
                    }
                    // AND binds tighter than OR, and concat tighter than AND.
                    let group = matches!(
                        (kind, operand),
                        (
                            _,
                            Node::Operator {
                                kind: OperatorKind::Or,
                                ..
                            }
                        ) | (
                            OperatorKind::Concat,
                            Node::Operator {
                                kind: OperatorKind::And,
                                ..
                            }
                        )
                    );
                    if group {
                        write!(f, "({operand})")?;
                    } else {
                        write!(f, "{operand}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

fn needs_quotes(value: &str) -> bool {
    value.starts_with(['"', '\''])
        || value.chars().any(char::is_whitespace)
        || !parser::balanced(value)
}

fn write_quoted(f: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in value.chars() {
        match c {
            '"' | '\\' => write!(f, "\\{c}")?,
            _ => write!(f, "{c}")?,
        }
    }
    f.write_str("\"")
}

/// Writes `value` between `delim`s so that the parser reads it back
/// unchanged: the delimiter, newlines, tabs and carriage returns are
/// escaped, and a backslash only where it would start an escape.
fn write_delimited(f: &mut fmt::Formatter<'_>, value: &str, delim: char) -> fmt::Result {
    write!(f, "{delim}")?;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // A regexp keeps its escapes as written, other than the delimiter's.
            '\\' if delim == '/' => match chars.next() {
                Some(n) => write!(f, "\\{n}")?,
                None => f.write_str("\\\\")?,
            },
            '\\' => match chars.peek() {
                None => f.write_str("\\\\")?,
                Some(&n) if n == delim || matches!(n, '\\' | 'n' | 'r' | 't') => {
                    f.write_str("\\\\")?
                }
                Some(_) => f.write_str("\\")?,
            },
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if c == delim => write!(f, "\\{c}")?,
            c => write!(f, "{c}")?,
        }
    }
    write!(f, "{delim}")
}
