//! Pattern trees with regexp sources.
//!
//! [`PatternSpec`] is a planner pattern lowered to regexps (or, for
//! structural queries, a template): the form sent to repo nodes and handed
//! to a [`crate::index::SearchIndex`]. Both search backends compile it into
//! the same matcher, so they agree on what matches.

use sourcegull_query::{Node, OperatorKind, Pattern, PatternKind, PatternType};

use crate::PatternInfo;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("invalid regular expression {pattern:?}: {message}")]
    Regex { pattern: String, message: String },
    #[error("invalid structural pattern: {0}")]
    Structural(String),
}

/// A pattern tree with regexp sources, before compilation. This is the form
/// sent to repo nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternSpec {
    Atom {
        regex: String,
        negated: bool,
    },
    /// A structural template and its `rule:`.
    Structural {
        template: String,
        rule: Option<String>,
    },
    And(Vec<PatternSpec>),
    Or(Vec<PatternSpec>),
}

impl PatternSpec {
    /// Lowers a planner pattern to regexp sources, or a structural
    /// template for structural queries.
    pub fn from_pattern(info: &PatternInfo) -> Result<PatternSpec, CompileError> {
        if info.pattern_type == PatternType::Structural {
            return match &info.pattern {
                Node::Pattern(p) if !p.negated => Ok(PatternSpec::Structural {
                    template: p.value.clone(),
                    rule: info.rule.clone(),
                }),
                _ => Err(CompileError::Structural("expected one template".to_owned())),
            };
        }
        Ok(lower(&info.pattern, info.pattern_type))
    }
}

fn lower(node: &Node, ty: PatternType) -> PatternSpec {
    match node {
        Node::Pattern(p) => PatternSpec::Atom {
            regex: atom_regex(p, ty),
            negated: p.negated,
        },
        // Parameters never reach the pattern tree; treat a stray one as true.
        Node::Parameter(_) => PatternSpec::And(Vec::new()),
        Node::Operator {
            kind: OperatorKind::Concat,
            operands,
            ..
        } if ty != PatternType::Keyword => {
            // Runs of adjacent, non-negated patterns are merged as the
            // pattern type says (`concat_run`); the rest is ANDed.
            let mut children = Vec::new();
            let mut run = Vec::new();
            for operand in operands {
                match operand {
                    Node::Pattern(p) if !p.negated => run.push(p),
                    _ => {
                        concat_run(&mut run, ty, &mut children);
                        children.push(lower(operand, ty));
                    }
                }
            }
            concat_run(&mut run, ty, &mut children);
            if children.len() == 1 {
                children.pop().expect("one child")
            } else {
                PatternSpec::And(children)
            }
        }
        Node::Operator { kind, operands, .. } => {
            let children = operands.iter().map(|o| lower(o, ty)).collect();
            match kind {
                OperatorKind::Or => PatternSpec::Or(children),
                OperatorKind::And | OperatorKind::Concat => PatternSpec::And(children),
            }
        }
    }
}

/// The regexp source of one pattern.
fn atom_regex(p: &Pattern, ty: PatternType) -> String {
    let regexp = p.kind == PatternKind::Regexp
        || (ty == PatternType::Regexp && p.kind != PatternKind::Quoted);
    if regexp && ty == PatternType::Regexp && p.kind == PatternKind::Literal {
        escape_parens(&p.value)
    } else if regexp {
        p.value.clone()
    } else {
        regex::escape(&p.value)
    }
}

/// Escapes the parentheses of a regexp query's pattern that are meant literally.
///
/// `()` becomes `\(\)` and a `(` ending the pattern becomes `\(`; escaped
/// characters and every other parenthesis are left as written.
fn escape_parens(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '(' if chars.peek() == Some(&')') => {
                chars.next();
                out.push_str("\\(\\)");
            }
            '(' if chars.peek().is_none() => out.push_str("\\("),
            c => out.push(c),
        }
    }
    out
}

/// Lowers a run of adjacent patterns in a concat into `out` and drains it.
///
/// A single pattern is one atom. In a regexp query several patterns are one
/// atom matching them in order with anything between them. Otherwise
/// adjacent non-regexp patterns form a phrase (their values joined on a
/// space), and each regexp pattern is an atom of its own.
fn concat_run(run: &mut Vec<&Pattern>, ty: PatternType, out: &mut Vec<PatternSpec>) {
    let atom = |regex: String| PatternSpec::Atom {
        regex,
        negated: false,
    };
    match run.as_slice() {
        [] => {}
        [p] => out.push(atom(atom_regex(p, ty))),
        patterns if ty == PatternType::Regexp => {
            let groups: Vec<String> = patterns
                .iter()
                .map(|p| format!("(?:{})", atom_regex(p, ty)))
                .collect();
            out.push(atom(groups.join(".*?")));
        }
        patterns => {
            let mut phrase: Vec<&str> = Vec::new();
            for p in patterns {
                if p.kind == PatternKind::Regexp {
                    if !phrase.is_empty() {
                        out.push(atom(regex::escape(&phrase.join(" "))));
                        phrase.clear();
                    }
                    out.push(atom(p.value.clone()));
                } else {
                    phrase.push(&p.value);
                }
            }
            if !phrase.is_empty() {
                out.push(atom(regex::escape(&phrase.join(" "))));
            }
        }
    }
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(q: &str, pattern_type: PatternType) -> PatternSpec {
        let root = sourcegull_query::parse(q, pattern_type)
            .unwrap()
            .root
            .unwrap();
        PatternSpec::from_pattern(&PatternInfo {
            pattern: root,
            pattern_type,
            case_sensitive: false,
            rule: None,
        })
        .unwrap()
    }

    fn atom(regex: &str) -> PatternSpec {
        PatternSpec::Atom {
            regex: regex.into(),
            negated: false,
        }
    }

    fn lit(s: &str) -> PatternSpec {
        atom(&regex::escape(s))
    }

    /// Adjacent literal patterns are one phrase, joined on a space; patterns
    /// next to a `/regexp/` are ANDed with it.
    #[test]
    fn standard_concatenation() {
        let std = |q: &str| spec(q, PatternType::Standard);
        assert_eq!(std("copyright (c) 2005"), lit("copyright (c) 2005"));
        assert_eq!(std("foo (bar)"), lit("foo (bar)"));
        assert_eq!(std("foo()"), lit("foo()"));
        assert_eq!(std("func(a,  b)"), lit("func(a,  b)"));
        assert_eq!(std("/a/ /b/"), PatternSpec::And(vec![atom("a"), atom("b")]));
        assert_eq!(
            std("a b not c d"),
            PatternSpec::And(vec![
                lit("a b"),
                PatternSpec::Atom {
                    regex: "c".into(),
                    negated: true
                },
                lit("d"),
            ])
        );
        assert_eq!(
            std("(x or y) and z"),
            PatternSpec::And(vec![PatternSpec::Or(vec![lit("x"), lit("y")]), lit("z")])
        );
    }

    #[test]
    fn literal_and_keyword_concatenation() {
        assert_eq!(spec("a (b) c", PatternType::Literal), lit("a (b) c"));
        assert_eq!(
            spec("copyright (c) 2005", PatternType::Keyword),
            PatternSpec::And(vec![lit("copyright"), lit("c"), lit("2005")])
        );
    }

    /// Regexp queries join adjacent patterns with `.*?`, in order.
    #[test]
    fn regexp_concatenation() {
        let re = |q: &str| spec(q, PatternType::Regexp);
        assert_eq!(re(r"foo\d+"), atom(r"foo\d+"));
        assert_eq!(re(r"foo bar\d"), atom(r"(?:foo).*?(?:bar\d)"));
        assert_eq!(re("(a|b) c"), atom("(?:(a|b)).*?(?:c)"));
        assert_eq!(re("foo() bar("), atom(r"(?:foo\(\)).*?(?:bar\()"));
        assert_eq!(
            re("a or b c"),
            PatternSpec::Or(vec![atom("a"), atom("(?:b).*?(?:c)")])
        );
        // Quoted patterns are literal, escaped into the joined regexp, and
        // keep their parentheses.
        assert_eq!(re(r#""foo.bar()""#), lit("foo.bar()"));
        assert_eq!(re("'a b'"), lit("a b"));
        assert_eq!(
            re(r#"foo "a.b(" bar\d"#),
            atom(r"(?:foo).*?(?:a\.b\().*?(?:bar\d)")
        );
        assert_eq!(re(r#"foo"bar""#), atom(r#"foo"bar""#));
        // `/.../` is a regexp, `//` the literal text.
        assert_eq!(re(r"/a\/b+/"), atom("a/b+"));
        assert_eq!(re("// x"), atom("(?://).*?(?:x)"));
    }

    #[test]
    fn escapes_literal_parentheses() {
        for (input, want) in [
            ("call()", r"call\(\)"),
            ("open(", r"open\("),
            ("a()b()c", r"a\(\)b\(\)c"),
            ("()(", r"\(\)\("),
            ("((", r"(\("),
            ("( )", "( )"),
            ("f(x)", "f(x)"),
            (r"x\(", r"x\("),
            (r"y\\(", r"y\\\("),
            ("ab\\", "ab\\"),
            ("é()", r"é\(\)"),
            (".*(z", ".*(z"),
        ] {
            assert_eq!(escape_parens(input), want, "{input:?}");
        }
    }

    #[test]
    fn concat_runs() {
        let re = |q: &str| spec(q, PatternType::Regexp);
        assert_eq!(re("alpha beta"), atom("(?:alpha).*?(?:beta)"));
        assert_eq!(re("get() set("), atom(r"(?:get\(\)).*?(?:set\()"));
        assert_eq!(
            re(r#"one "t.w" three"#),
            atom(r"(?:one).*?(?:t\.w).*?(?:three)")
        );
        assert_eq!(re(r"only\d"), atom(r"only\d"));
        let std = |q: &str| spec(q, PatternType::Standard);
        assert_eq!(
            std("/red/ green blue"),
            PatternSpec::And(vec![atom("red"), lit("green blue")])
        );
        assert_eq!(
            std("green /red/ blue /x+/"),
            PatternSpec::And(vec![lit("green"), atom("red"), lit("blue"), atom("x+")])
        );
        assert_eq!(std("/p/ /q/"), PatternSpec::And(vec![atom("p"), atom("q")]));
        assert_eq!(spec("k (v) w", PatternType::Literal), lit("k (v) w"));
        assert_eq!(
            std("m n not o p"),
            PatternSpec::And(vec![
                lit("m n"),
                PatternSpec::Atom {
                    regex: "o".into(),
                    negated: true
                },
                lit("p"),
            ])
        );
    }
}
