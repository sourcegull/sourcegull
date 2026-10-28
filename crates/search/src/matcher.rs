//! The content matcher: a boolean tree of regexps.
//!
//! Unindexed search evaluates the same tree the index evaluates, so both
//! backends agree on what matches. A file matches when the tree is true;
//! the returned ranges are those of the non-negated atoms that matched.
//! [`Matcher::find_hits`] also says which atom matched each range, for
//! ranking. [`PatternSpec`] is the uncompiled tree.
//!
//! A structural template is an atom too; its matches depend on
//! the file's language (strings and comments), which the `*_in` methods
//! take.
//!
//! Case-insensitive regexps without a literal prefix (`\w+_lock\(`) make
//! the regex engine scan with its DFA at about 1 GB/s, since the case
//! variants of their literal parts leave it no single literal to look for.
//! Such atoms also get a case-sensitive regexp ([`folded`]) that matches the
//! ASCII-lowercased file in the same places; it finds its literal with
//! `memmem` (over 10 GB/s). Files holding one of the two non-ASCII letters
//! that fold to ASCII ones (KELVIN SIGN, LONG S) use the unfolded regexp.

use std::cell::OnceCell;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
use regex_syntax::hir::{self, Class, Hir, HirKind};

use crate::ranking::Hit;
use crate::{CompileError, PatternInfo, PatternSpec};

#[derive(Debug, Clone)]
pub enum Matcher {
    /// `id` numbers the non-negated atoms in tree order (0 for negated ones).
    Atom {
        regex: Regex,
        /// The same match on the ASCII-lowercased content, for
        /// case-insensitive regexps that are slow to scan ([`folded`]).
        folded: Option<Regex>,
        negated: bool,
        id: usize,
    },
    Structural {
        pattern: Arc<sourcegull_structural::Pattern>,
        id: usize,
    },
    And(Vec<Matcher>),
    Or(Vec<Matcher>),
}

/// A non-negated atom of a [`Matcher`].
#[derive(Debug, Clone, Copy)]
pub enum AtomRef<'m> {
    Regex(&'m Regex),
    Structural(&'m sourcegull_structural::Pattern),
}

impl Matcher {
    pub fn compile(info: &PatternInfo) -> Result<Matcher, CompileError> {
        Matcher::from_spec(&PatternSpec::from_pattern(info)?, info.case_sensitive)
    }

    pub fn from_spec(spec: &PatternSpec, case_sensitive: bool) -> Result<Matcher, CompileError> {
        Matcher::build_tree(spec, case_sensitive, &mut 0)
    }

    fn build_tree(
        spec: &PatternSpec,
        case_sensitive: bool,
        next: &mut usize,
    ) -> Result<Matcher, CompileError> {
        let mut all = |ss: &[PatternSpec]| {
            ss.iter()
                .map(|s| Matcher::build_tree(s, case_sensitive, next))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(match spec {
            PatternSpec::Atom { regex, negated } => {
                let id = if *negated { 0 } else { *next };
                *next += usize::from(!*negated);
                Matcher::Atom {
                    regex: build(regex, case_sensitive)?,
                    folded: (!case_sensitive).then(|| folded(regex)).flatten(),
                    negated: *negated,
                    id,
                }
            }
            PatternSpec::Structural { template, rule } => {
                let id = *next;
                *next += 1;
                let pattern = sourcegull_structural::Pattern::new(template, rule.as_deref())
                    .map_err(|e| CompileError::Structural(e.to_string()))?;
                Matcher::Structural {
                    pattern: Arc::new(pattern),
                    id,
                }
            }
            PatternSpec::And(ss) => Matcher::And(all(ss)?),
            PatternSpec::Or(ss) => Matcher::Or(all(ss)?),
        })
    }

    /// The non-negated atoms in `id` order.
    pub fn atoms(&self) -> Vec<AtomRef<'_>> {
        fn walk<'m>(m: &'m Matcher, out: &mut Vec<AtomRef<'m>>) {
            match m {
                Matcher::Atom { negated: true, .. } => {}
                Matcher::Atom { regex, .. } => out.push(AtomRef::Regex(regex)),
                Matcher::Structural { pattern, .. } => out.push(AtomRef::Structural(pattern)),
                Matcher::And(ms) | Matcher::Or(ms) => ms.iter().for_each(|m| walk(m, out)),
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }

    /// Whether the tree holds a structural template, whose matches depend
    /// on the file's language.
    pub fn is_structural(&self) -> bool {
        match self {
            Matcher::Structural { .. } => true,
            Matcher::Atom { .. } => false,
            Matcher::And(ms) | Matcher::Or(ms) => ms.iter().any(Matcher::is_structural),
        }
    }

    /// The sorted, merged match ranges if `content` matches, else `None`.
    /// A match made only of negations gives `Some` with no ranges.
    pub fn find(&self, content: &[u8]) -> Option<Vec<Range<usize>>> {
        self.find_in(content, None)
    }

    /// [`Matcher::find`] in a file of language `lang` (a `sourcegull-languages`
    /// id), for structural templates.
    pub fn find_in(&self, content: &[u8], lang: Option<&str>) -> Option<Vec<Range<usize>>> {
        self.find_hits_in(content, lang, usize::MAX)
            .map(|hits| merge(&hits))
    }

    /// The matches of the non-negated atoms if `content` matches, sorted
    /// by start, with the atom that made each; ranges of different atoms
    /// may overlap.
    pub fn find_hits(&self, content: &[u8]) -> Option<Vec<Hit>> {
        self.find_hits_in(content, None, usize::MAX)
    }

    /// [`Matcher::find_hits`] in a file of language `lang`, with at most
    /// `cap` hits per atom.
    pub fn find_hits_in(&self, content: &[u8], lang: Option<&str>, cap: usize) -> Option<Vec<Hit>> {
        let mut hits = Vec::new();
        if !self.eval(&Haystack::new(content), lang, cap, &mut hits) {
            return None;
        }
        hits.sort_by_key(|h| (h.range.start, h.range.end));
        Some(hits)
    }

    /// True if the tree matches `content`, without collecting ranges.
    pub fn is_match(&self, content: &[u8]) -> bool {
        self.is_match_in(&Haystack::new(content))
    }

    fn is_match_in(&self, hay: &Haystack<'_>) -> bool {
        match self {
            Matcher::Atom { negated, .. } => {
                let (regex, text) = self.regex_for(hay);
                regex.is_match(text) != *negated
            }
            Matcher::Structural { pattern, .. } => {
                !pattern.find_iter(hay.content, None, 1).is_empty()
            }
            Matcher::And(ms) => ms.iter().all(|m| m.is_match_in(hay)),
            Matcher::Or(ms) => ms.iter().any(|m| m.is_match_in(hay)),
        }
    }

    /// An atom's regexp and the text to run it on: the folded regexp on
    /// the lowercased content when it has one and the content allows it.
    fn regex_for<'h>(&'h self, hay: &'h Haystack<'_>) -> (&'h Regex, &'h [u8]) {
        let Matcher::Atom { regex, folded, .. } = self else {
            unreachable!("only atoms have regexps");
        };
        match folded.as_ref().zip(hay.lower()) {
            Some((f, lower)) => (f, lower),
            None => (regex, hay.content),
        }
    }

    fn eval(&self, hay: &Haystack<'_>, lang: Option<&str>, cap: usize, out: &mut Vec<Hit>) -> bool {
        let content = hay.content;
        match self {
            Matcher::Structural { pattern, id } => {
                let before = out.len();
                out.extend(
                    pattern
                        .find_iter(content, lang, cap)
                        .into_iter()
                        .map(|m| Hit {
                            range: m.range,
                            atom: *id,
                        }),
                );
                out.len() > before
            }
            // A negated atom holds where its regexp does not match.
            Matcher::Atom { negated: true, .. } => self.is_match_in(hay),
            Matcher::Atom {
                negated: false, id, ..
            } => {
                let (regex, text) = self.regex_for(hay);
                // An empty-width match (e.g. `^`) still matches the file.
                let mut matched = false;
                let mut n = 0;
                for m in regex.find_iter(text) {
                    matched = true;
                    if m.is_empty() {
                        continue;
                    }
                    if n == cap {
                        break;
                    }
                    n += 1;
                    out.push(Hit {
                        range: m.range(),
                        atom: *id,
                    });
                }
                matched
            }
            Matcher::And(ms) => {
                let mark = out.len();
                for m in ms {
                    if !m.eval(hay, lang, cap, out) {
                        out.truncate(mark);
                        return false;
                    }
                }
                true
            }
            Matcher::Or(ms) => {
                let mut any = false;
                for m in ms {
                    any |= m.eval(hay, lang, cap, out);
                }
                any
            }
        }
    }
}

/// Sorted ranges merged where they overlap.
pub fn merge(hits: &[Hit]) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(hits.len());
    for h in hits {
        match merged.last_mut() {
            Some(last) if h.range.start < last.end => last.end = last.end.max(h.range.end),
            _ => merged.push(h.range.clone()),
        }
    }
    merged
}

/// A file's content, with its ASCII-lowercased copy made when a folded
/// regexp first needs it.
struct Haystack<'c> {
    content: &'c [u8],
    lower: OnceCell<Option<Vec<u8>>>,
}

/// UTF-8 for the two non-ASCII letters whose case folds include ASCII
/// letters: KELVIN SIGN (k) and LATIN SMALL LETTER LONG S (s).
static NON_ASCII_FOLDS: LazyLock<[memmem::Finder<'static>; 2]> = LazyLock::new(|| {
    [
        memmem::Finder::new("\u{212A}".as_bytes()),
        memmem::Finder::new("\u{17F}".as_bytes()),
    ]
});

impl<'c> Haystack<'c> {
    fn new(content: &'c [u8]) -> Self {
        Haystack {
            content,
            lower: OnceCell::new(),
        }
    }

    /// The content with ASCII letters lowercased (offsets unchanged), or
    /// `None` when it holds a letter that folds to an ASCII one.
    fn lower(&self) -> Option<&[u8]> {
        self.lower
            .get_or_init(|| {
                let folds = NON_ASCII_FOLDS
                    .iter()
                    .any(|f| f.find(self.content).is_some());
                (!folds).then(|| self.content.to_ascii_lowercase())
            })
            .as_deref()
    }
}

/// For a case-insensitive regexp with no literal prefix (which the regex
/// engine would scan for), a case-sensitive regexp that matches the
/// ASCII-lowercased text exactly where the given regexp matches the text, as
/// long as the text holds neither KELVIN SIGN nor LONG S: every class the
/// case-insensitive parse produced loses its ASCII capitals and those two
/// letters. `None` when that is not needed or not sound (a case-sensitive
/// part, `(?-i:A)`, would no longer match).
fn folded(source: &str) -> Option<Regex> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(true)
        .multi_line(true)
        .utf8(false)
        .build()
        .parse(source)
        .ok()?;
    let prefixes = hir::literal::Extractor::new()
        .kind(hir::literal::ExtractKind::Prefix)
        .extract(&hir);
    // A short set of prefixes is found with a SIMD prefilter already.
    if prefixes.len().is_some_and(|n| n <= 64) && prefixes.min_literal_len().is_some_and(|n| n >= 3)
    {
        return None;
    }
    let lowered = lower_hir(&hir)?;
    if lowered == hir {
        return None;
    }
    RegexBuilder::new(&lowered.to_string())
        .multi_line(true)
        .build()
        .ok()
}

/// [`folded`]'s rewrite of one node.
fn lower_hir(h: &Hir) -> Option<Hir> {
    Some(match h.kind() {
        HirKind::Empty | HirKind::Look(_) => h.clone(),
        // Letters in a literal are case-sensitive ones (`(?-i:a)`): they
        // would match either case once the text is lowercased.
        HirKind::Literal(l) => {
            if l.0.iter().any(u8::is_ascii_alphabetic) {
                return None;
            }
            h.clone()
        }
        HirKind::Class(Class::Unicode(c)) => {
            let has = |ch: char| c.ranges().iter().any(|r| r.start() <= ch && ch <= r.end());
            // Only classes holding both cases of each ASCII letter or neither.
            if ('A'..='Z').any(|u| has(u) != has(u.to_ascii_lowercase())) {
                return None;
            }
            let mut c = c.clone();
            c.difference(&hir::ClassUnicode::new([
                hir::ClassUnicodeRange::new('A', 'Z'),
                hir::ClassUnicodeRange::new('\u{17F}', '\u{17F}'),
                hir::ClassUnicodeRange::new('\u{212A}', '\u{212A}'),
            ]));
            Hir::class(Class::Unicode(c))
        }
        HirKind::Class(Class::Bytes(c)) => {
            let has = |b: u8| c.ranges().iter().any(|r| r.start() <= b && b <= r.end());
            if (b'A'..=b'Z').any(|u| has(u) != has(u.to_ascii_lowercase())) {
                return None;
            }
            let mut c = c.clone();
            c.difference(&hir::ClassBytes::new([hir::ClassBytesRange::new(
                b'A', b'Z',
            )]));
            Hir::class(Class::Bytes(c))
        }
        HirKind::Repetition(r) => Hir::repetition(hir::Repetition {
            sub: Box::new(lower_hir(&r.sub)?),
            ..r.clone()
        }),
        HirKind::Capture(c) => Hir::capture(hir::Capture {
            sub: Box::new(lower_hir(&c.sub)?),
            ..c.clone()
        }),
        HirKind::Concat(hs) => Hir::concat(hs.iter().map(lower_hir).collect::<Option<_>>()?),
        HirKind::Alternation(hs) => {
            Hir::alternation(hs.iter().map(lower_hir).collect::<Option<_>>()?)
        }
    })
}

fn build(source: &str, case: bool) -> Result<Regex, CompileError> {
    RegexBuilder::new(source)
        .case_insensitive(!case)
        .multi_line(true)
        .build()
        .map_err(|e| CompileError::Regex {
            pattern: source.to_owned(),
            message: e.to_string(),
        })
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::{Job, Limits, plan_query};
    use sourcegull_query::PatternType;

    fn matcher(q: &str) -> Matcher {
        let job = plan_query(
            q,
            PatternType::Keyword,
            &Limits::default(),
            chrono::Utc::now(),
        )
        .unwrap();
        fn find(j: &Job) -> Option<&PatternInfo> {
            match j {
                Job::Content { pattern, .. } => Some(pattern),
                Job::Parallel(js) => js.iter().find_map(find),
                Job::Select { child, .. }
                | Job::Limit { child, .. }
                | Job::Timeout { child, .. } => find(child),
                _ => None,
            }
        }
        Matcher::compile(find(&job).expect("content job")).unwrap()
    }

    #[test]
    fn keyword_terms_are_anded() {
        let m = matcher("foo bar");
        assert_eq!(m.find(b"foo x bar"), Some(vec![0..3, 6..9]));
        assert_eq!(m.find(b"foo only"), None);
    }

    #[test]
    fn or_and_negation() {
        let m = matcher("foo or bar");
        assert_eq!(m.find(b"bar"), Some(vec![0..3]));
        let m = matcher("foo not bar");
        assert!(m.is_match(b"foo"));
        assert!(!m.is_match(b"foo bar"));
    }

    #[test]
    fn case_and_regexp() {
        assert!(matcher("FOO").is_match(b"foo"));
        assert!(!matcher("FOO case:yes").is_match(b"foo"));
        let m = matcher("patterntype:regexp fo+ ");
        assert_eq!(m.find(b"a fooo"), Some(vec![2..6]));
    }

    #[test]
    fn literal_phrase_and_merge() {
        let m = matcher("patterntype:literal foo bar");
        assert!(m.is_match(b"foo bar"));
        assert!(!m.is_match(b"bar foo"));
        let m = matcher("patterntype:regexp ab|bc");
        assert_eq!(m.find(b"abc"), Some(vec![0..2]));
    }

    #[test]
    fn structural_atoms() {
        let m = Matcher::from_spec(
            &PatternSpec::Structural {
                template: "f(:[x])".into(),
                rule: None,
            },
            false,
        )
        .unwrap();
        assert!(m.is_structural());
        assert_eq!(m.find(b"a f(b) f(\")\")"), Some(vec![2..6, 7..13]));
        // In Go, the parenthesis inside the string does not count.
        assert_eq!(m.find_in(b"f(\")\") // f(1)", Some("go")), Some(vec![0..6]));
        assert!(
            Matcher::from_spec(
                &PatternSpec::Structural {
                    template: "f(:[x".into(),
                    rule: None,
                },
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn folded_regexps_match_like_the_unfolded_ones() {
        let texts: &[&str] = &[
            "spin_LOCK(x); Foo_Lock( mutex_lock(&m)\nRAW_SPIN_lock(",
            "a_lo\u{212A}( b_LOCK( and \u{17F}pin_lock(",
            "mixed CASE: Error ERROR error eRrOr; ünïcödé_lock(",
            "",
        ];
        for pat in [
            r"\w+_lock\(",
            r"[a-z_]+_LOCK\(",
            r"\w+_lo",
            r".*error",
            r"\bspin\w*",
            r"[^a-z]+_lock",
            r"\w+(?-i:_lock)\(",
            r"\w+(?-i:[a-z])_lock",
            r"\w+_[\x41-\x5a]",
        ] {
            let m = Matcher::from_spec(
                &PatternSpec::Atom {
                    regex: pat.into(),
                    negated: false,
                },
                false,
            )
            .unwrap();
            let Matcher::Atom { regex, folded, .. } = &m else {
                unreachable!()
            };
            if pat.contains("?-i") {
                assert!(folded.is_none(), "{pat}");
            }
            for text in texts {
                let want: Vec<_> = regex
                    .find_iter(text.as_bytes())
                    .filter(|m| !m.is_empty())
                    .map(|m| m.range())
                    .collect();
                let got = m.find(text.as_bytes()).unwrap_or_default();
                assert_eq!(merge_ranges(want), got, "{pat} on {text:?}");
            }
        }
        // Prefixes the regex engine scans for itself are left alone.
        assert!(folded("error").is_none());
        assert!(folded(r"\w+_lock\(").is_some());
        assert!(folded(r"_+").is_none());
    }

    proptest::proptest! {
        #[test]
        fn folded_regexps_agree_on_random_input(
            parts in proptest::collection::vec(
                (
                    proptest::sample::select(vec![
                        "a", "B", "k", "S", "_", r"\w", "[a-c]", "[A-C]", "[^a]", ".", r"\b",
                        "(?-i:a)", "(?-i:[a-z])", r"\s", "é", "(a|K)",
                    ]),
                    proptest::sample::select(vec!["", "+", "*", "?"]),
                ),
                1..6,
            ),
            text in proptest::collection::vec(
                proptest::sample::select(vec![
                    'a', 'A', 'b', 'B', 'k', 'K', 's', 'S', '_', ' ', '\n', '\u{212A}',
                    '\u{17F}', 'é', 'É', 'x',
                ]),
                0..40,
            ),
        ) {
            let pat: String = parts
                .iter()
                .map(|(p, q)| if p == &r"\b" { (*p).to_owned() } else { format!("{p}{q}") })
                .collect();
            let text: String = text.into_iter().collect();
            let m = Matcher::from_spec(
                &PatternSpec::Atom {
                    regex: pat.clone(),
                    negated: false,
                },
                false,
            )
            .unwrap();
            let Matcher::Atom { regex, .. } = &m else {
                unreachable!()
            };
            let want: Vec<_> = regex
                .find_iter(text.as_bytes())
                .filter(|m| !m.is_empty())
                .map(|m| m.range())
                .collect();
            let matched = regex.is_match(text.as_bytes());
            let got = m.find(text.as_bytes());
            proptest::prop_assert_eq!(got.is_some(), matched, "{} on {:?}", pat, text);
            proptest::prop_assert_eq!(got.unwrap_or_default(), merge_ranges(want), "{} on {:?}", pat, text);
        }
    }

    fn merge_ranges(rs: Vec<Range<usize>>) -> Vec<Range<usize>> {
        let hits: Vec<Hit> = rs.into_iter().map(|range| Hit { range, atom: 0 }).collect();
        merge(&hits)
    }

    #[test]
    fn hits_name_their_atoms() {
        let m = matcher("foo or (bar not baz) or oba");
        assert_eq!(m.atoms().len(), 3);
        assert!(!m.is_structural());
        let hits = m.find_hits(b"foobar").unwrap();
        let got: Vec<_> = hits.iter().map(|h| (h.range.clone(), h.atom)).collect();
        assert_eq!(got, [(0..3, 0), (2..5, 2), (3..6, 1)]);
        assert_eq!(m.find(b"foobar"), Some(vec![0..6]));
    }
}
