//! File and repository scores.
//!
//! Both search backends score each matching file with [`file_score`], and
//! `web` adds [`repo_score`] before ordering results. A file's query score
//! is its best match, where a match on a symbol definition counts most,
//! then whole-word, then partial matches, plus a share for each pattern
//! atom that matched and for atoms
//! matching the file name. Keyword queries ([`Mode::Bm25`]) use BM25 over
//! the pattern's terms instead: term frequencies from the matches (symbol
//! definitions and file names weighted up), document frequencies estimated
//! from trigram posting lengths, lengths from file sizes.
//!
//! Test, vendored and generated files are scaled down. Static parts break
//! ties: the file's static rank from the code graph ([`doc_rank_score`],
//! indexed search only), shallower paths, then the repository's rank. The
//! static parts only order documents within a query score: they add at most
//! a few dozen points while match kinds are hundreds apart. Scores are only
//! compared within one search.

use std::ops::Range;

/// A match on a whole symbol name at its definition.
pub const SYMBOL: f64 = 7000.0;
/// A match inside a symbol name at its definition.
pub const SYMBOL_PARTIAL: f64 = 4000.0;
/// Extra for a symbol definition of a type, function or method.
pub const SYMBOL_KIND: f64 = 100.0;
/// A match with word boundaries on both sides.
pub const WORD: f64 = 500.0;
/// A match with a word boundary on one side.
pub const PARTIAL_WORD: f64 = 50.0;
/// A case-insensitive match whose text equals the pattern exactly.
pub const EXACT_CASE: f64 = 25.0;
/// Times the fraction of the pattern's atoms that matched.
pub const ATOM_RATIO: f64 = 400.0;
/// Times the fraction of the pattern's atoms matching the file name.
pub const FILE_NAME: f64 = 1000.0;
/// Times BM25.
pub const BM25_SCALE: f64 = 1000.0;
/// The most [`depth_score`] adds.
pub const DEPTH: f64 = 10.0;
/// The most [`repo_score`] adds.
pub const REPO_RANK: f64 = 20.0;
/// The most [`doc_rank_score`] adds.
pub const DOC_RANK: f64 = 20.0;

/// Factors for files that are rarely what a search is after.
pub const TEST_FACTOR: f64 = 0.75;
pub const VENDORED_FACTOR: f64 = 0.4;
pub const GENERATED_FACTOR: f64 = 0.4;

/// BM25 parameters (the usual defaults).
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Term-frequency weight of a match on a symbol definition and of an atom
/// matching the file name.
const TF_SYMBOL: f64 = 5.0;
const TF_FILE_NAME: f64 = 5.0;
/// Term-frequency weight of a match that is not a whole word.
const TF_PARTIAL: f64 = 0.5;

/// A match of a pattern atom in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub range: Range<usize>,
    /// The atom's index among the pattern's non-negated atoms.
    pub atom: usize,
}

/// A non-negated pattern atom.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Atom {
    /// The text the atom matches when it is a plain literal, for
    /// [`EXACT_CASE`] on case-insensitive searches.
    pub literal: Option<Vec<u8>>,
    /// Documents that may contain the atom ([`Mode::Bm25`]).
    pub doc_freq: u64,
}

/// A symbol definition in the file: the byte range of its name and its LSP
/// kind number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub range: Range<usize>,
    pub kind: u8,
}

/// Document statistics of the files a search covers ([`Mode::Bm25`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corpus {
    pub docs: u64,
    pub avg_len: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Best match, atom ratio and file name (standard, literal and regexp
    /// queries).
    Matches,
    /// BM25 over the atoms (keyword queries).
    Bm25(Corpus),
}

/// What a file's score is computed from.
#[derive(Debug, Clone, Copy)]
pub struct FileSignals<'a> {
    pub path: &'a str,
    pub content: &'a [u8],
    /// Content matches, sorted by start.
    pub hits: &'a [Hit],
    pub atoms: &'a [Atom],
    /// Per atom, how it matches the file's base name ([`name_weight`]).
    pub name_matches: &'a [f64],
    /// Symbol definitions in the file, sorted by start; empty when unknown.
    pub definitions: &'a [Definition],
    pub case_sensitive: bool,
}

/// The score of a file with content matches.
pub fn file_score(f: &FileSignals<'_>, mode: Mode) -> f64 {
    let query = match mode {
        Mode::Matches => match_score(f),
        Mode::Bm25(corpus) => bm25(f, corpus),
    };
    query * path_factor(f.path) + depth_score(f.path)
}

/// The score of a file found by its path alone (path search): the path is
/// scored as content, with whole-word matches in the base name counting as
/// [`FILE_NAME`].
pub fn path_score(path: &str, hits: &[Hit], atoms: usize) -> f64 {
    let base = base_name_start(path);
    let best = hits
        .iter()
        .map(|h| {
            let w = word_score(path.as_bytes(), &h.range);
            if h.range.start >= base {
                w + FILE_NAME
            } else {
                w
            }
        })
        .fold(0.0, f64::max);
    (best + atom_ratio(hits, atoms)) * path_factor(path) + depth_score(path)
}

/// The score of a symbol search result: its best symbol, by how much of the
/// name the pattern covers and the symbol's kind.
pub fn symbol_score(path: &str, symbols: impl IntoIterator<Item = (bool, u8)>) -> f64 {
    let best = symbols
        .into_iter()
        .map(|(whole, kind)| {
            let s = if whole { SYMBOL } else { SYMBOL_PARTIAL };
            s + kind_bonus(kind)
        })
        .fold(0.0, f64::max);
    best * path_factor(path) + depth_score(path)
}

/// How an atom matches a file's base name: 1 for a whole word, 0.1 for
/// part of one, 0 when `ranges` (its matches in `name`) is empty.
pub fn name_weight(name: &[u8], ranges: impl IntoIterator<Item = Range<usize>>) -> f64 {
    ranges
        .into_iter()
        .map(|r| {
            if word_score(name, &r) == WORD {
                1.0
            } else {
                0.1
            }
        })
        .fold(0.0, f64::max)
}

/// The tie-breaking share of a repository's stars, up to [`REPO_RANK`].
pub fn repo_score(stars: i32) -> f64 {
    let stars = f64::from(stars.max(0));
    REPO_RANK * ((stars + 1.0).log10() / 6.0).min(1.0)
}

/// Up to [`DOC_RANK`]: the share of a file's static rank (in [0, 1], from
/// PageRank over the code graph).
pub fn doc_rank_score(rank: f32) -> f64 {
    DOC_RANK * f64::from(rank.clamp(0.0, 1.0))
}

/// Up to [`DEPTH`]: files nearer the root first.
pub fn depth_score(path: &str) -> f64 {
    let depth = path.matches('/').count() as f64;
    DEPTH / (1.0 + depth)
}

fn kind_bonus(kind: u8) -> f64 {
    // Class, method, constructor, enum, interface, function, struct.
    if matches!(kind, 5 | 6 | 9 | 10 | 11 | 12 | 23) {
        SYMBOL_KIND
    } else {
        0.0
    }
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// [`WORD`] for a match bounded by non-word bytes on both sides,
/// [`PARTIAL_WORD`] for one side.
fn word_score(content: &[u8], r: &Range<usize>) -> f64 {
    let before = r.start == 0 || !is_word(content[r.start - 1]);
    let after = r.end >= content.len() || !is_word(content[r.end]);
    match (before, after) {
        (true, true) => WORD,
        (true, false) | (false, true) => PARTIAL_WORD,
        (false, false) => 0.0,
    }
}

/// The definition whose name `r` overlaps, and whether `r` covers it.
fn definition<'d>(defs: &'d [Definition], r: &Range<usize>) -> Option<(&'d Definition, bool)> {
    let i = defs.partition_point(|d| d.range.end <= r.start);
    let d = defs.get(i)?;
    (d.range.start < r.end).then_some((d, r.start <= d.range.start && d.range.end <= r.end))
}

fn hit_score(f: &FileSignals<'_>, h: &Hit) -> f64 {
    let mut s = word_score(f.content, &h.range);
    if let Some((d, whole)) = definition(f.definitions, &h.range) {
        s += if whole { SYMBOL } else { SYMBOL_PARTIAL } + kind_bonus(d.kind);
    }
    if !f.case_sensitive
        && let Some(lit) = f.atoms.get(h.atom).and_then(|a| a.literal.as_deref())
        && f.content.get(h.range.clone()) == Some(lit)
    {
        s += EXACT_CASE;
    }
    s
}

fn atom_ratio(hits: &[Hit], atoms: usize) -> f64 {
    if atoms == 0 {
        return 0.0;
    }
    let mut seen = vec![false; atoms];
    for h in hits {
        if let Some(s) = seen.get_mut(h.atom) {
            *s = true;
        }
    }
    ATOM_RATIO * seen.iter().filter(|s| **s).count() as f64 / atoms as f64
}

fn match_score(f: &FileSignals<'_>) -> f64 {
    let best = f.hits.iter().map(|h| hit_score(f, h)).fold(0.0, f64::max);
    let names: f64 = f.name_matches.iter().sum();
    let name = match f.atoms.len() {
        0 => 0.0,
        n => FILE_NAME * names / n as f64,
    };
    best + atom_ratio(f.hits, f.atoms.len()) + name
}

fn bm25(f: &FileSignals<'_>, corpus: Corpus) -> f64 {
    let mut tf = vec![0.0f64; f.atoms.len()];
    for h in f.hits {
        let Some(t) = tf.get_mut(h.atom) else {
            continue;
        };
        *t += match definition(f.definitions, &h.range) {
            Some(_) => TF_SYMBOL,
            None if word_score(f.content, &h.range) == WORD => 1.0,
            None => TF_PARTIAL,
        };
    }
    for (t, named) in tf.iter_mut().zip(f.name_matches) {
        *t += TF_FILE_NAME * named;
    }
    let n = corpus.docs.max(1) as f64;
    let len = f.content.len() as f64 / corpus.avg_len.max(1.0);
    let score: f64 = tf
        .iter()
        .zip(f.atoms)
        .filter(|(t, _)| **t > 0.0)
        .map(|(t, a)| {
            let df = (a.doc_freq as f64).min(n);
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            idf * t * (K1 + 1.0) / (t + K1 * (1.0 - B + B * len))
        })
        .sum();
    BM25_SCALE * score
}

/// The byte offset where the base name starts.
pub fn base_name_start(path: &str) -> usize {
    path.rfind('/').map_or(0, |i| i + 1)
}

/// The factor for the kind of file `path` is: [`TEST_FACTOR`],
/// [`VENDORED_FACTOR`], [`GENERATED_FACTOR`] or 1.
pub fn path_factor(path: &str) -> f64 {
    let lower = path.to_ascii_lowercase();
    let mut f = 1.0;
    if is_vendored(&lower) {
        f *= VENDORED_FACTOR;
    }
    if is_generated(&lower) {
        f *= GENERATED_FACTOR;
    }
    if is_test(&lower) {
        f *= TEST_FACTOR;
    }
    f
}

fn has_dir(path: &str, dirs: &[&str]) -> bool {
    path.split('/')
        .rev()
        .skip(1)
        .any(|segment| dirs.contains(&segment))
}

fn is_vendored(path: &str) -> bool {
    has_dir(
        path,
        &[
            "vendor",
            "vendors",
            "node_modules",
            "bower_components",
            "third_party",
            "third-party",
            "thirdparty",
        ],
    )
}

fn is_generated(path: &str) -> bool {
    let name = &path[base_name_start(path)..];
    const SUFFIXES: [&str; 8] = [
        ".pb.go", "_pb2.py", ".pb.cc", ".pb.h", ".min.js", ".min.css", ".map", ".snap",
    ];
    const NAMES: [&str; 7] = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "cargo.lock",
        "go.sum",
        "poetry.lock",
        "composer.lock",
    ];
    SUFFIXES.iter().any(|s| name.ends_with(s))
        || NAMES.contains(&name)
        || name.contains("_generated.")
        || name.contains(".generated.")
        || name.starts_with("zz_generated")
        || has_dir(path, &["generated", "__generated__"])
}

fn is_test(path: &str) -> bool {
    let name = &path[base_name_start(path)..];
    let stem = name.split('.').next().unwrap_or(name);
    has_dir(
        path,
        &["test", "tests", "__tests__", "testdata", "spec", "specs"],
    ) || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with("test") && stem.len() > 4 && !stem.ends_with("latest")
        || stem.starts_with("test_")
        || name.contains(".test.")
        || name.contains(".spec.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(range: Range<usize>) -> Hit {
        Hit { range, atom: 0 }
    }

    fn signals<'a>(
        path: &'a str,
        content: &'a [u8],
        hits: &'a [Hit],
        atoms: &'a [Atom],
        defs: &'a [Definition],
    ) -> FileSignals<'a> {
        FileSignals {
            path,
            content,
            hits,
            atoms,
            name_matches: &[0.0],
            definitions: defs,
            case_sensitive: false,
        }
    }

    #[test]
    fn definitions_beat_words_beat_partials() {
        let atoms = [Atom {
            literal: Some(b"Run".to_vec()),
            doc_freq: 1,
        }];
        let def = [Definition {
            range: 5..8,
            kind: 12,
        }];
        let text = b"func Run() {}";
        let defined = file_score(
            &signals("a.go", text, &[hit(5..8)], &atoms, &def),
            Mode::Matches,
        );
        let word = file_score(
            &signals("a.go", text, &[hit(5..8)], &atoms, &[]),
            Mode::Matches,
        );
        let partial = file_score(
            &signals("a.go", b"Runner", &[hit(0..3)], &atoms, &[]),
            Mode::Matches,
        );
        let inside = file_score(
            &signals("a.go", b"xRunx", &[hit(1..4)], &atoms, &[]),
            Mode::Matches,
        );
        assert!(defined > word && word > partial && partial > inside);
        assert_eq!(defined - word, SYMBOL + SYMBOL_KIND);
        // Exact case only counts on case-insensitive searches.
        let folded = file_score(
            &signals("a.go", b"run", &[hit(0..3)], &atoms, &[]),
            Mode::Matches,
        );
        let exact = file_score(
            &signals("a.go", b"Run", &[hit(0..3)], &atoms, &[]),
            Mode::Matches,
        );
        assert_eq!(exact - folded, EXACT_CASE);
    }

    #[test]
    fn atom_ratio_and_file_names() {
        let atoms = [Atom::default(), Atom::default()];
        let both = [
            hit(0..3),
            Hit {
                range: 4..7,
                atom: 1,
            },
        ];
        let one = [hit(0..3)];
        let s = |hits: &[Hit], names: &[f64]| {
            file_score(
                &FileSignals {
                    path: "src/foo.rs",
                    content: b"foo bar",
                    hits,
                    atoms: &atoms,
                    name_matches: names,
                    definitions: &[],
                    case_sensitive: true,
                },
                Mode::Matches,
            )
        };
        assert_eq!(s(&both, &[0.0, 0.0]) - s(&one, &[0.0, 0.0]), 200.0);
        assert_eq!(s(&one, &[1.0, 0.0]) - s(&one, &[0.0, 0.0]), 500.0);
        assert_eq!(name_weight(b"foo.rs", Some(0..3)), 1.0);
        assert_eq!(name_weight(b"foos.rs", Some(0..3)), 0.1);
        assert_eq!(name_weight(b"bar.rs", []), 0.0);
    }

    #[test]
    fn bm25_counts_terms_and_rarity() {
        let corpus = Corpus {
            docs: 1000,
            avg_len: 100.0,
        };
        let common = [Atom {
            literal: None,
            doc_freq: 900,
        }];
        let rare = [Atom {
            literal: None,
            doc_freq: 3,
        }];
        let text = b"foo foo foo and more text to be about one hundred bytes long...........";
        let s = |hits: &[Hit], atoms: &[Atom]| {
            file_score(&signals("a", text, hits, atoms, &[]), Mode::Bm25(corpus))
        };
        let once = [hit(0..3)];
        let thrice = [hit(0..3), hit(4..7), hit(8..11)];
        assert!(s(&thrice, &common) > s(&once, &common));
        assert!(s(&once, &rare) > s(&thrice, &common));
        // A definition counts as several plain matches.
        let def = [Definition {
            range: 0..3,
            kind: 13,
        }];
        let with_def = file_score(
            &signals("a", text, &once, &common, &def),
            Mode::Bm25(corpus),
        );
        assert!(with_def > s(&thrice, &common));
    }

    #[test]
    fn path_kinds() {
        assert_eq!(path_factor("src/server.go"), 1.0);
        assert_eq!(path_factor("src/server_test.go"), TEST_FACTOR);
        assert_eq!(path_factor("tests/it.rs"), TEST_FACTOR);
        assert_eq!(path_factor("web/app.spec.ts"), TEST_FACTOR);
        assert_eq!(path_factor("test_api.py"), TEST_FACTOR);
        assert_eq!(path_factor("FooTest.java"), TEST_FACTOR);
        assert_eq!(path_factor("latest.txt"), 1.0);
        assert_eq!(path_factor("contest/x.go"), 1.0);
        assert_eq!(path_factor("vendor/github.com/x/y.go"), VENDORED_FACTOR);
        assert_eq!(path_factor("api/types.pb.go"), GENERATED_FACTOR);
        assert_eq!(path_factor("Cargo.lock"), GENERATED_FACTOR);
        assert_eq!(
            path_factor("node_modules/x/index.test.js"),
            VENDORED_FACTOR * TEST_FACTOR
        );
    }

    #[test]
    fn static_parts() {
        assert!(depth_score("a.go") > depth_score("x/a.go"));
        assert_eq!(repo_score(0), 0.0);
        assert!(repo_score(10) < repo_score(1000));
        assert_eq!(repo_score(10_000_000), REPO_RANK);
        assert_eq!(doc_rank_score(0.0), 0.0);
        assert_eq!(doc_rank_score(1.0), DOC_RANK);
        assert!(doc_rank_score(1.0) < PARTIAL_WORD);
        assert_eq!(
            path_score("cmd/server/server.go", &[hit(11..17)], 1),
            WORD + FILE_NAME + ATOM_RATIO + depth_score("cmd/server/server.go")
        );
    }
}
