//! sourcegull-structural: structural search.
//!
//! A [`Template`] with holes matches source text whose delimiters balance
//! the same way, with strings and comments per language ([`syntax`]).
//! [`Pattern`] is a template plus an optional [`Rule`].
//!
//! Each file is scanned once into a skip table (delimiter pairs, string
//! and comment spans), so the matcher steps over balanced groups and
//! strings in O(1). Matches are leftmost and non-overlapping and do not
//! start inside strings or comments. A template matches inside words
//! (`foo` matches in `xfoox`).
//!
//! Templates are trimmed, a match that starts with a `:[x]` hole does not
//! start on whitespace, empty matches are dropped, and same-named holes and
//! rules compare text without surrounding whitespace. Word delimiters
//! (`case`/`esac`), `nested` rules and holes inside string literals of the
//! template are not supported.

mod matcher;
mod rule;
mod scan;
pub mod syntax;
mod template;

use std::ops::Range;

pub use matcher::Env;
pub use rule::Rule;
pub use syntax::{Syntax, syntax_for};
pub use template::{Hole, HoleKind, Node, Template, TemplateError};

use matcher::{Ctx, End};
use scan::{Kind, Scan, char_len};

/// Matcher steps one file may take; past it, the search of that file
/// stops with the matches found so far.
const FILE_BUDGET: u64 = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StructuralError {
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error("invalid rule: {0}")]
    Rule(String),
}

/// One match: where it is and what each hole matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub range: Range<usize>,
    pub env: Env,
}

/// A template and an optional rule.
#[derive(Debug, Clone)]
pub struct Pattern {
    template: Template,
    rule: Option<Rule>,
}

impl Pattern {
    pub fn new(template: &str, rule: Option<&str>) -> Result<Pattern, StructuralError> {
        Ok(Pattern {
            template: Template::parse(template)?,
            rule: rule
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(Rule::parse)
                .transpose()?,
        })
    }

    pub fn template(&self) -> &Template {
        &self.template
    }

    /// Text every match contains: the template's literal parts and the
    /// texts its rule requires holes to equal. For prefiltering.
    pub fn literals(&self) -> Vec<String> {
        let mut out = self.template.literals();
        if let Some(r) = &self.rule {
            out.extend(r.literals());
        }
        out
    }

    /// The matches in `src`, a file in language `lang` (a `sourcegull-languages`
    /// id; `None` or unknown: generic syntax), up to `limit`.
    pub fn find_iter(&self, src: &[u8], lang: Option<&str>, limit: usize) -> Vec<Match> {
        let syntax = lang.map_or(&syntax::GENERIC, syntax_for);
        let nodes = &self.template.nodes;
        // A file without the template's first literal cannot match.
        let first_lit = match nodes.first() {
            Some(Node::Lit(l)) => Some(l.as_slice()),
            _ => None,
        };
        if self
            .literals()
            .iter()
            .any(|l| memchr::memmem::find(src, l.as_bytes()).is_none())
        {
            return Vec::new();
        }
        let scan = Scan::new(src, syntax);
        // A match starting with a `:[x]` hole does not start on whitespace;
        // one starting with an identifier hole starts a word.
        let first = match nodes.first() {
            Some(Node::Hole(h)) => Some(&h.kind),
            _ => None,
        };
        let skip_ws = matches!(first, Some(HoleKind::Everything));
        let starts_word = matches!(first, Some(HoleKind::Identifier));
        // Matches start where the first literal is, or, when the first node
        // is a hole the rule pins to a text, where that text is.
        let pinned = match (nodes.first(), &self.rule) {
            (Some(Node::Hole(h)), Some(r)) if h.named() => r.literal_for(&h.name),
            _ => None,
        };
        let finder = first_lit
            .map(<[u8]>::to_vec)
            .or_else(|| pinned.map(String::into_bytes))
            .map(|n| memchr::memmem::Finder::new(&n).into_owned());
        let mut out = Vec::new();
        let mut pos = 0;
        let mut steps = 0u64;
        while pos < src.len() && out.len() < limit && steps < FILE_BUDGET {
            if let Some(f) = &finder {
                match f.find(&src[pos..]) {
                    Some(k) => pos += k,
                    None => break,
                }
            }
            let b = src[pos];
            let skip = scan.kind(pos) == Kind::Inside
                || (skip_ws && b.is_ascii_whitespace())
                || (starts_word && pos > 0 && matcher::is_word(src[pos - 1]));
            if !skip && let Some(m) = self.match_at(src, &scan, pos, &mut steps) {
                pos = m.range.end.max(pos + 1);
                out.push(m);
                continue;
            }
            pos += char_len(b);
        }
        out
    }

    /// The match starting at `pos`, if any.
    fn match_at(&self, src: &[u8], scan: &Scan, pos: usize, steps: &mut u64) -> Option<Match> {
        let mut ctx = Ctx {
            src,
            scan,
            steps: 0,
            start: pos,
        };
        let rule = self.rule.as_ref();
        let mut accept =
            |end: usize, env: &Env| end > pos && rule.is_none_or(|r| r.holds(src, env));
        let mut end = End::Free {
            limit: src.len(),
            accept: &mut accept,
        };
        let mut env = Env::new();
        let e = ctx.seq(&self.template.nodes, 0, pos, &mut end, &mut env);
        *steps += u64::from(ctx.steps);
        Some(Match {
            range: pos..e?,
            env,
        })
    }
}

/// The bindings (name and text) when `t` matches all of `text` (rule
/// `match` cases); surrounding whitespace is ignored.
pub(crate) fn matches_whole(
    t: &Template,
    text: &[u8],
    syntax: &Syntax,
) -> Option<Vec<(String, String)>> {
    let scan = Scan::new(text, syntax);
    let start = text.len() - text.trim_ascii_start().len();
    let mut ctx = Ctx {
        src: text,
        scan: &scan,
        steps: 0,
        start,
    };
    let mut end = End::Anchored(start + text[start..].trim_ascii_end().len());
    let mut env = Env::new();
    ctx.seq(&t.nodes, 0, start, &mut end, &mut env)?;
    Some(
        env.into_iter()
            .map(|(n, r)| {
                let v = String::from_utf8_lossy(matcher::trimmed(text, &r)).into_owned();
                (n, v)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matched texts, and the bindings of the first match.
    fn find(t: &str, rule: Option<&str>, lang: &str, src: &str) -> Vec<String> {
        let p = Pattern::new(t, rule).unwrap();
        p.find_iter(src.as_bytes(), Some(lang), usize::MAX)
            .into_iter()
            .map(|m| src[m.range].to_owned())
            .collect()
    }

    fn holes(t: &str, src: &str) -> Vec<(String, String)> {
        let p = Pattern::new(t, None).unwrap();
        let m = p.find_iter(src.as_bytes(), Some("go"), 1);
        m[0].env
            .iter()
            .map(|(n, r)| (n.clone(), src[r.clone()].trim().to_owned()))
            .collect()
    }

    #[test]
    fn balanced_holes() {
        let src = "x := fmt.Println(a, f(b, c), \"d)\")\nfmt.Println()\n";
        assert_eq!(
            find("fmt.Println(:[args])", None, "go", src),
            ["fmt.Println(a, f(b, c), \"d)\")", "fmt.Println()"]
        );
        assert_eq!(
            holes("fmt.Println(:[a], :[rest])", src),
            [
                ("a".into(), "a".into()),
                ("rest".into(), "f(b, c), \"d)\"".into())
            ]
        );
        // A hole cannot leave its group.
        assert!(find("f(:[x]) c", None, "go", "g(f(a) b) c").is_empty());
    }

    #[test]
    fn whitespace_and_comments() {
        let src = "if  err != nil {\n\treturn err\n}";
        assert_eq!(
            find("if err != nil { return :[e] }", None, "go", src),
            [src]
        );
        // Template whitespace needs whitespace or a comment; elsewhere
        // whitespace is significant.
        assert!(find("a ( b )", None, "go", "a(b)").is_empty());
        assert!(find("a(b)", None, "go", "a ( b )").is_empty());
        assert!(find("foo bar", None, "go", "foobar").is_empty());
        assert_eq!(find("a b", None, "go", "a/* c */b"), ["a/* c */b"]);
        // Comments before a literal are skipped; holes do not take them.
        assert_eq!(find("f(a)", None, "go", "f(/**/a) f( a)"), ["f(/**/a)"]);
        assert_eq!(
            holes("match :[x] end", "match /* c */ this /**/ end"),
            [("x".into(), "this".into())]
        );
    }

    #[test]
    fn trailing_holes_take_the_rest() {
        assert_eq!(find("a :[x]", None, "go", "a b c\nd"), ["a b c\nd"]);
        assert_eq!(find("a :[x]", None, "go", "(a b) c"), ["a b"]);
        assert_eq!(find(":[x.]", None, "go", "fo.o(x)"), ["fo.o", "x"]);
        assert_eq!(
            holes("return :[e:e]", "return a.b(c, d)[0] + 1"),
            [("e".into(), "a.b(c, d)[0]".into())]
        );
    }

    #[test]
    fn hole_kinds() {
        assert_eq!(
            holes(":[[f]](:[[x]])", "call(arg)"),
            [("f".into(), "call".into()), ("x".into(), "arg".into())]
        );
        assert_eq!(
            holes("return :[v.];", "return a.b.c;"),
            [("v".into(), "a.b.c".into())]
        );
        assert_eq!(
            holes("// :[c\\n]", "x // note here\ny"),
            [("c".into(), "note here".into())]
        );
        assert_eq!(
            find("x = :[n~\\d+]", None, "go", "x = 42; x = y"),
            ["x = 42"]
        );
        assert_eq!(find("a:[ s]b", None, "go", "a  b ab"), ["a  b"]);
        assert_eq!(find("a ... b", None, "go", "a (x) b"), ["a (x) b"]);
    }

    #[test]
    fn line_holes() {
        let p = Pattern::new("note::[c\\n]", None).unwrap();
        let src = b"note: here\nnext";
        let m = p.find_iter(src, None, 10);
        assert_eq!(m[0].range, 0..11);
    }

    #[test]
    fn same_name_means_same_text() {
        let src = "f(a, a) f(a, b) f( x , x)";
        assert_eq!(
            find("f(:[x], :[x])", None, "go", src),
            ["f(a, a)", "f( x , x)"]
        );
        assert_eq!(find("f(:[_], :[_])", None, "go", src).len(), 3);
    }

    #[test]
    fn strings_comments_and_words() {
        // Templates match inside words, except that an
        // identifier hole starts a word.
        let src = "// foo(1)\ns := \"foo(2)\"\nfoo(3)\nbarfoo(4)";
        assert_eq!(find("foo(:[x])", None, "go", src), ["foo(3)", "foo(4)"]);
        assert_eq!(find(":[[f]](4)", None, "go", src), ["barfoo(4)"]);
        // Unknown languages still pair delimiters.
        assert_eq!(find("[:[x]]", None, "unknown", "a[1][2]"), ["[1]", "[2]"]);
        // Python strings and comments.
        let py = "x = '(' # )\nprint(x)";
        assert_eq!(find("print(:[a])", None, "python", py), ["print(x)"]);
    }

    #[test]
    fn rules() {
        let src = "f(a, a) f(a, b) f(1, 2)";
        assert_eq!(
            find("f(:[x], :[y])", Some("where :[x] == :[y]"), "go", src),
            ["f(a, a)"]
        );
        assert_eq!(
            find(
                "f(:[x], :[y])",
                Some("where :[x] != :[y], :[y] != \"2\""),
                "go",
                src
            ),
            ["f(a, b)"]
        );
        assert_eq!(
            find(
                "f(:[x], :[y])",
                Some("where match :[x] { | \":[~\\d+]\" -> true | \"a\" -> false }"),
                "go",
                src
            ),
            ["f(1, 2)"]
        );
        assert_eq!(
            find(
                "f(:[[x]], :[y])",
                Some("where \"<:[[x]]>\" == \"<a>\""),
                "go",
                src
            ),
            ["f(a, a)", "f(a, b)"]
        );
        let p = Pattern::new(
            ":[[f]](:[x])",
            Some("where :[f] == \"panic\", \"y\" == :[x]"),
        )
        .unwrap();
        assert_eq!(p.literals(), ["panic", "y"]);
        assert_eq!(
            find(
                ":[[f]](:[x])",
                Some("where :[f] == \"panic\""),
                "go",
                "xpanic(1) panic(2)"
            ),
            ["panic(2)"]
        );
        assert!(Pattern::new("x", Some("where rewrite :[x] { \"a\" -> \"b\" }")).is_err());
        assert!(Pattern::new("x", Some(":[x] == :[y]")).is_err());
        assert!(Pattern::new("x", Some("where :[x] =")).is_err());
    }

    #[test]
    fn pathological_templates_stop() {
        let src = "y ".to_owned() + &"a ".repeat(3000) + "x";
        let p = Pattern::new(":[a] :[b] :[c] :[d] x y", None).unwrap();
        let t = std::time::Instant::now();
        assert!(p.find_iter(src.as_bytes(), None, 10).is_empty());
        assert!(t.elapsed() < std::time::Duration::from_secs(20));
    }
}
