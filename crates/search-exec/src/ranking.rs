//! Scores for matches on repo nodes.
//!
//! [`Scorer`] holds what the score model in [`sourcegull_search::ranking`]
//! needs from the pattern: its non-negated atoms, their literal text and,
//! for BM25, each atom's trigram query so document frequencies can be
//! estimated from a shard's posting lengths. Unindexed search has no
//! corpus statistics and uses fixed ones, so BM25 still saturates term
//! frequencies there but treats every term as equally rare.

use std::cmp::Ordering;

use regex::bytes::Regex;
use regex_syntax::hir::HirKind;
use sourcegull_index_format::{FormatError, Shard};
use sourcegull_search::matcher::AtomRef;
use sourcegull_search::ranking::{self, Atom, Corpus, Definition, FileSignals, Hit, Mode};
use sourcegull_search::{Matcher, PatternSpec};

use crate::trigrams::{TrigramQuery, extract};

/// Matching files verified per search on a node before ranking stops
/// looking for better ones (`count:` above this raises it).
pub const RANK_CAP: usize = 10_000;

/// Corpus statistics assumed where none are known (unindexed search).
const UNINDEXED_CORPUS: Corpus = Corpus {
    docs: 10_000,
    avg_len: 8_000.0,
};

#[derive(Debug, Clone)]
pub struct Scorer {
    /// Per atom, its regexp (`None` for structural templates, which are not
    /// matched against file names).
    regexes: Vec<Option<Regex>>,
    literals: Vec<Option<Vec<u8>>>,
    trigrams: Vec<TrigramQuery>,
    case_sensitive: bool,
    bm25: bool,
}

/// A [`Scorer`] with the statistics of the shard (or commit) searched.
#[derive(Debug, Clone)]
pub struct Scoring<'s> {
    scorer: &'s Scorer,
    mode: Mode,
    atoms: Vec<Atom>,
}

impl Scorer {
    pub fn new(matcher: Option<&Matcher>, case_sensitive: bool, bm25: bool) -> Scorer {
        let atoms = matcher.map_or_else(Vec::new, Matcher::atoms);
        let regexes: Vec<Option<Regex>> = atoms
            .iter()
            .map(|a| match a {
                AtomRef::Regex(r) => Some((*r).clone()),
                AtomRef::Structural(_) => None,
            })
            .collect();
        let literals = regexes
            .iter()
            .map(|r| r.as_ref().and_then(|r| literal(r.as_str())))
            .collect();
        let trigrams = if bm25 {
            atoms
                .iter()
                .map(|a| match a {
                    AtomRef::Regex(r) => {
                        let atom = PatternSpec::Atom {
                            regex: r.as_str().to_owned(),
                            negated: false,
                        };
                        extract(&atom, case_sensitive)
                    }
                    AtomRef::Structural(p) => crate::trigrams::literals(&p.literals()),
                })
                .collect()
        } else {
            Vec::new()
        };
        Scorer {
            regexes,
            literals,
            trigrams,
            case_sensitive,
            bm25,
        }
    }

    /// Scoring with `shard`'s statistics.
    pub fn for_shard(&self, shard: &Shard) -> Result<Scoring<'_>, FormatError> {
        if !self.bm25 {
            return self.scoring(Mode::Matches, |_| Ok(0));
        }
        let (docs, bytes) = shard.content_stats();
        let corpus = Corpus {
            docs,
            avg_len: bytes as f64 / docs.max(1) as f64,
        };
        self.scoring(Mode::Bm25(corpus), |i| {
            crate::indexed::estimate(&self.trigrams[i], shard).map(|n| n.min(docs))
        })
    }

    /// Scoring without index statistics.
    pub fn unindexed(&self) -> Scoring<'_> {
        let mode = if self.bm25 {
            Mode::Bm25(UNINDEXED_CORPUS)
        } else {
            Mode::Matches
        };
        let df = UNINDEXED_CORPUS.docs / 10;
        self.scoring(mode, |_| Ok::<_, FormatError>(df))
            .expect("infallible")
    }

    fn scoring<E>(
        &self,
        mode: Mode,
        mut doc_freq: impl FnMut(usize) -> Result<u64, E>,
    ) -> Result<Scoring<'_>, E> {
        let atoms = (0..self.regexes.len())
            .map(|i| {
                Ok(Atom {
                    literal: self.literals[i].clone(),
                    doc_freq: doc_freq(i)?,
                })
            })
            .collect::<Result<_, E>>()?;
        Ok(Scoring {
            scorer: self,
            mode,
            atoms,
        })
    }
}

impl Scoring<'_> {
    /// The score of a file with content matches `hits` and symbol
    /// definitions `defs` (empty when unknown).
    pub fn content(&self, path: &str, content: &[u8], hits: &[Hit], defs: &[Definition]) -> f64 {
        let base = &path.as_bytes()[ranking::base_name_start(path)..];
        let name_matches: Vec<f64> = self
            .scorer
            .regexes
            .iter()
            .map(|r| {
                r.as_ref().map_or(0.0, |r| {
                    ranking::name_weight(base, r.find_iter(base).map(|m| m.range()))
                })
            })
            .collect();
        ranking::file_score(
            &FileSignals {
                path,
                content,
                hits,
                atoms: &self.atoms,
                name_matches: &name_matches,
                definitions: defs,
                case_sensitive: self.scorer.case_sensitive,
            },
            self.mode,
        )
    }
}

/// The literal a regexp matches when it is a plain string.
fn literal(source: &str) -> Option<Vec<u8>> {
    let hir = regex_syntax::Parser::new().parse(source).ok()?;
    match hir.kind() {
        HirKind::Literal(l) => Some(l.0.to_vec()),
        _ => None,
    }
}

/// The symbol definitions of a doc (kinds and name ranges, as
/// [`Shard::symbol_ranges`] reads them), sorted by position.
pub fn definitions(symbols: Vec<(u8, std::ops::Range<u32>)>) -> Vec<Definition> {
    let mut defs: Vec<Definition> = symbols
        .into_iter()
        .map(|(kind, r)| Definition {
            range: r.start as usize..r.end as usize,
            kind,
        })
        .collect();
    defs.sort_by_key(|d| (d.range.start, d.range.end));
    defs
}

/// The score of a path search result from its path ranges.
pub fn path_score(path: &str, ranges: &[std::ops::Range<usize>]) -> f64 {
    let hits: Vec<Hit> = ranges
        .iter()
        .map(|r| Hit {
            range: r.clone(),
            atom: 0,
        })
        .collect();
    ranking::path_score(path, &hits, 1)
}

/// The score of a symbol search result: `whole` says whether the pattern
/// covers each symbol's whole name.
pub fn symbols_score<'a>(
    path: &str,
    matcher: Option<&Matcher>,
    symbols: impl IntoIterator<Item = (&'a str, u8)>,
) -> f64 {
    ranking::symbol_score(
        path,
        symbols.into_iter().map(|(name, kind)| {
            let whole = matcher.is_none_or(|m| {
                m.find(name.as_bytes())
                    .is_some_and(|r| r.first().is_some_and(|r| *r == (0..name.len())))
            });
            (whole, kind)
        }),
    )
}

/// Higher scores first.
pub fn by_score(a: f64, b: f64) -> Ordering {
    b.total_cmp(&a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(literal("foo"), Some(b"foo".to_vec()));
        assert_eq!(literal(&regex::escape("a.b")), Some(b"a.b".to_vec()));
        assert_eq!(literal("fo+"), None);
    }

    #[test]
    fn unindexed_scores_rank_definitions_and_names() {
        let m = Matcher::from_spec(
            &PatternSpec::Atom {
                regex: "server".into(),
                negated: false,
            },
            false,
        )
        .unwrap();
        let scorer = Scorer::new(Some(&m), false, true);
        let s = scorer.unindexed();
        let text = b"type Server struct{}";
        let hits = m.find_hits(text).unwrap();
        let def = [Definition {
            range: 5..11,
            kind: 23,
        }];
        let in_name = s.content("server.go", text, &hits, &[]);
        let plain = s.content("main.go", text, &hits, &[]);
        let defined = s.content("main.go", text, &hits, &def);
        assert!(in_name > plain);
        assert!(defined > plain);
        assert!(s.content("main_test.go", text, &hits, &[]) < plain);
    }

    #[test]
    fn symbol_scores() {
        let m = Matcher::from_spec(
            &PatternSpec::Atom {
                regex: "run".into(),
                negated: false,
            },
            false,
        )
        .unwrap();
        let whole = symbols_score("a.go", Some(&m), [("Run", 12)]);
        let part = symbols_score("a.go", Some(&m), [("Runner", 12)]);
        assert!(whole > part);
    }
}
