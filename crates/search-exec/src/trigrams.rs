//! Trigram queries from patterns.
//!
//! A [`TrigramQuery`] is a boolean tree of trigrams that every file matching
//! the pattern must satisfy. It is extracted from the `regex-syntax` HIR,
//! after the codesearch approach (Russ Cox, "Regular Expression Matching
//! with a Trigram Index"), simplified: each node yields either a small set
//! of exact strings or a query, and runs of exact strings are joined across
//! a concatenation. Strings are case-folded like the index (ASCII only);
//! case-insensitive classes such as `[Kk\u{212A}]` stay sets of variants.
//! Negated atoms never prune. The query only narrows candidates: the
//! matcher still verifies each one.

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind};
use sourcegull_index_format::fold;
use sourcegull_search::PatternSpec;

/// The most exact strings kept for one node before it becomes a query.
const MAX_SET: usize = 64;
/// The most characters in a class expanded into exact strings.
const MAX_CLASS: u32 = 16;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrigramQuery {
    /// Every file may match.
    All,
    /// No file can match.
    Never,
    Trigram(u32),
    And(Vec<TrigramQuery>),
    Or(Vec<TrigramQuery>),
}

impl TrigramQuery {
    /// Whether a file whose sorted, distinct (folded) trigrams are `have`
    /// may match.
    pub fn may_match(&self, have: &[u32]) -> bool {
        match self {
            TrigramQuery::All => true,
            TrigramQuery::Never => false,
            TrigramQuery::Trigram(t) => have.binary_search(t).is_ok(),
            TrigramQuery::And(qs) => qs.iter().all(|q| q.may_match(have)),
            TrigramQuery::Or(qs) => qs.iter().any(|q| q.may_match(have)),
        }
    }

    pub fn and(qs: impl IntoIterator<Item = TrigramQuery>) -> TrigramQuery {
        let mut out = BTreeSet::new();
        for q in qs {
            match q {
                TrigramQuery::All => {}
                TrigramQuery::Never => return TrigramQuery::Never,
                TrigramQuery::And(inner) => out.extend(inner),
                q => {
                    out.insert(q);
                }
            }
        }
        match out.len() {
            0 => TrigramQuery::All,
            1 => out.pop_first().expect("one"),
            _ => TrigramQuery::And(out.into_iter().collect()),
        }
    }

    pub fn or(qs: impl IntoIterator<Item = TrigramQuery>) -> TrigramQuery {
        let mut out = BTreeSet::new();
        for q in qs {
            match q {
                TrigramQuery::All => return TrigramQuery::All,
                TrigramQuery::Never => {}
                TrigramQuery::Or(inner) => out.extend(inner),
                q => {
                    out.insert(q);
                }
            }
        }
        match out.len() {
            0 => TrigramQuery::Never,
            1 => out.pop_first().expect("one"),
            _ => TrigramQuery::Or(out.into_iter().collect()),
        }
    }

    /// The trigrams of a folded string, ANDed; too short a string is `All`.
    fn string(s: &[u8]) -> TrigramQuery {
        TrigramQuery::and(s.windows(3).map(|w| {
            TrigramQuery::Trigram(
                (u32::from(w[0]) << 16) | (u32::from(w[1]) << 8) | u32::from(w[2]),
            )
        }))
    }
}

/// The trigram query for a pattern tree, compiled as the matcher compiles it.
pub fn extract(spec: &PatternSpec, case_sensitive: bool) -> TrigramQuery {
    match spec {
        PatternSpec::Atom { negated: true, .. } => TrigramQuery::All,
        PatternSpec::Atom { regex, .. } => atom(regex, case_sensitive),
        PatternSpec::Structural { template, rule } => {
            sourcegull_structural::Pattern::new(template, rule.as_deref())
                .map_or(TrigramQuery::All, |p| literals(&p.literals()))
        }
        PatternSpec::And(ss) => TrigramQuery::and(ss.iter().map(|s| extract(s, case_sensitive))),
        PatternSpec::Or(ss) => TrigramQuery::or(ss.iter().map(|s| extract(s, case_sensitive))),
    }
}

/// Files holding every one of `lits` (as written: structural templates
/// are case-sensitive).
pub fn literals(lits: &[String]) -> TrigramQuery {
    TrigramQuery::and(lits.iter().map(|l| atom(&regex::escape(l), true)))
}

fn atom(regex: &str, case_sensitive: bool) -> TrigramQuery {
    let parsed = regex_syntax::ParserBuilder::new()
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .utf8(false)
        .build()
        .parse(regex);
    match parsed {
        Ok(hir) => info(&hir).query(),
        // The matcher rejects it too; nothing to narrow.
        Err(_) => TrigramQuery::All,
    }
}

type Set = BTreeSet<Vec<u8>>;

enum Info {
    /// The node matches exactly one of these folded strings.
    Exact(Set),
    /// One or more repetitions of these strings.
    Repeat(Set),
    Query(TrigramQuery),
}

impl Info {
    fn query(self) -> TrigramQuery {
        match self {
            Info::Exact(set) | Info::Repeat(set) => {
                TrigramQuery::or(set.iter().map(|s| TrigramQuery::string(s)))
            }
            Info::Query(q) => q,
        }
    }
}

fn empty() -> Set {
    BTreeSet::from([Vec::new()])
}

fn cross(a: &Set, b: &Set) -> Option<Set> {
    if a.len() * b.len() > MAX_SET {
        return None;
    }
    Some(
        a.iter()
            .flat_map(|x| b.iter().map(move |y| [x.as_slice(), y].concat()))
            .collect(),
    )
}

fn info(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::Exact(empty()),
        HirKind::Literal(lit) => {
            Info::Exact(BTreeSet::from([lit.0.iter().map(|&b| fold(b)).collect()]))
        }
        HirKind::Class(class) => {
            class_set(class).map_or(Info::Query(TrigramQuery::All), Info::Exact)
        }
        HirKind::Capture(c) => info(&c.sub),
        HirKind::Repetition(rep) => {
            let sub = info(&rep.sub);
            match (rep.min, rep.max, sub) {
                (_, _, Info::Query(q)) if rep.min > 0 => Info::Query(q),
                (0, Some(1), Info::Exact(mut set)) if set.len() < MAX_SET => {
                    set.insert(Vec::new());
                    Info::Exact(set)
                }
                (0, ..) => Info::Query(TrigramQuery::All),
                (1, Some(1), sub) => sub,
                (_, _, Info::Exact(set) | Info::Repeat(set)) => Info::Repeat(set),
                (_, _, Info::Query(q)) => Info::Query(q),
            }
        }
        HirKind::Concat(parts) => {
            let mut acc = Vec::new();
            let mut cur = empty();
            let mut exact = true;
            for part in parts {
                match info(part) {
                    Info::Exact(set) => match cross(&cur, &set) {
                        Some(c) => cur = c,
                        None => {
                            acc.push(Info::Exact(cur).query());
                            cur = set;
                            exact = false;
                        }
                    },
                    // A match starts and ends with one of the strings.
                    Info::Repeat(set) => {
                        let joined = cross(&cur, &set).map(Info::Exact);
                        acc.push(joined.unwrap_or(Info::Exact(cur)).query());
                        acc.push(Info::Exact(set.clone()).query());
                        cur = set;
                        exact = false;
                    }
                    Info::Query(q) => {
                        acc.push(Info::Exact(cur).query());
                        acc.push(q);
                        cur = empty();
                        exact = false;
                    }
                }
            }
            if exact {
                return Info::Exact(cur);
            }
            acc.push(Info::Exact(cur).query());
            Info::Query(TrigramQuery::and(acc))
        }
        HirKind::Alternation(alts) => {
            let infos: Vec<Info> = alts.iter().map(info).collect();
            let total: usize = infos
                .iter()
                .map(|i| match i {
                    Info::Exact(s) => s.len(),
                    _ => MAX_SET + 1,
                })
                .sum();
            if total <= MAX_SET {
                let mut set = Set::new();
                for i in infos {
                    if let Info::Exact(s) = i {
                        set.extend(s);
                    }
                }
                return Info::Exact(set);
            }
            Info::Query(TrigramQuery::or(infos.into_iter().map(Info::query)))
        }
    }
}

/// The folded strings of a small class, or `None` if it is too large.
fn class_set(class: &Class) -> Option<Set> {
    let mut set = Set::new();
    match class {
        Class::Unicode(c) => {
            let total: u32 = c
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if total > MAX_CLASS * 4 {
                return None;
            }
            for r in c.ranges() {
                for ch in r.start()..=r.end() {
                    let mut buf = [0u8; 4];
                    let s = ch.encode_utf8(&mut buf);
                    set.insert(s.bytes().map(fold).collect());
                }
            }
        }
        Class::Bytes(c) => {
            let total: u32 = c
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1)
                .sum();
            if total > MAX_CLASS * 4 {
                return None;
            }
            for r in c.ranges() {
                for b in r.start()..=r.end() {
                    set.insert(vec![fold(b)]);
                }
            }
        }
    }
    (set.len() <= MAX_CLASS as usize).then_some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_index_format::trigram_bytes;

    fn t(s: &str) -> TrigramQuery {
        let b = s.as_bytes();
        TrigramQuery::Trigram(trigram_bytes([b[0], b[1], b[2]]))
    }

    fn q(re: &str, cs: bool) -> TrigramQuery {
        extract(
            &PatternSpec::Atom {
                regex: re.into(),
                negated: false,
            },
            cs,
        )
    }

    #[test]
    fn may_match_on_a_file_trigram_set() {
        let have = sourcegull_index_format::trigrams(b"func NewReader() {}");
        assert!(q("NewReader", true).may_match(&have));
        assert!(q("newreader", false).may_match(&have));
        assert!(q("(Reader|Writer)", true).may_match(&have));
        assert!(!q("NewWriter", true).may_match(&have));
        assert!(q(".*", true).may_match(&have));
    }

    #[test]
    fn literals_and_case() {
        let foobar = TrigramQuery::and([t("foo"), t("oob"), t("oba"), t("bar")]);
        assert_eq!(q("foobar", true), foobar);
        assert_eq!(q("FooBar", true), foobar);
        assert_eq!(q("foobar", false), foobar);
        assert_eq!(q("fo", false), TrigramQuery::All);
        assert_eq!(q("", false), TrigramQuery::All);
    }

    #[test]
    fn unicode_case_variants_stay() {
        // `k` folds to K, k and the Kelvin sign: two folded variants.
        let k = q("kab", false);
        let kelvin = TrigramQuery::string(&["\u{212A}".as_bytes(), b"ab"].concat());
        assert_eq!(k, TrigramQuery::or([t("kab"), kelvin]));
    }

    #[test]
    fn regex_structure() {
        assert_eq!(q("foo.*bar", true), TrigramQuery::and([t("foo"), t("bar")]));
        assert_eq!(q("abc|xyz", true), TrigramQuery::or([t("abc"), t("xyz")]));
        assert_eq!(
            q("a[bc]de", true),
            TrigramQuery::or([
                TrigramQuery::and([t("abd"), t("bde")]),
                TrigramQuery::and([t("acd"), t("cde")])
            ])
        );
        assert_eq!(q("x(abc)?y", true), TrigramQuery::All);
        assert_eq!(q("(abcd)+", true), TrigramQuery::and([t("abc"), t("bcd")]));
        assert_eq!(q("\\w+abc\\s", true), t("abc"));
        assert_eq!(q("^func abc", true), q("func abc", true));
        assert_eq!(q("[^\\x00-\\x{10FFFF}]", true), TrigramQuery::Never);
    }

    #[test]
    fn trees() {
        let spec = PatternSpec::And(vec![
            PatternSpec::Atom {
                regex: "abc".into(),
                negated: false,
            },
            PatternSpec::Atom {
                regex: "xyz".into(),
                negated: true,
            },
        ]);
        assert_eq!(extract(&spec, true), t("abc"));
        let spec = PatternSpec::Or(vec![
            PatternSpec::Atom {
                regex: "abc".into(),
                negated: false,
            },
            PatternSpec::Atom {
                regex: "xyz".into(),
                negated: true,
            },
        ]);
        assert_eq!(extract(&spec, true), TrigramQuery::All);
    }
}
