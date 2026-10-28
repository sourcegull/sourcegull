//! Unindexed search: walk a commit's tree and grep the blobs.
//!
//! Reads the object database directly (no archive). Path filters run before
//! any blob is read, and anchored `file:` prefixes prune whole subtrees;
//! binary files and files over the size limit are skipped. Blobs are
//! searched in parallel.
//!
//! Symbol search ([`search_commit_symbols`]) parses the files whose
//! language has a symbol grammar and whose trigrams may match the pattern.
//! Repo nodes cache parsed definitions by blob OID, language and extractor
//! version; direct callers can use the uncached entry point.

use std::ops::{ControlFlow, Range};

use regex::{Regex, RegexBuilder};
use sourcegull_gitstore::ObjectId;
use sourcegull_gitstore::parallel::{default_threads, map_ordered};
use sourcegull_gitstore::read::{
    DEFAULT_MAX_FILE_SIZE, ListTreeOptions, is_binary, list_tree_pruned, read_blob_by_oid,
};
use sourcegull_languages::Language;
use sourcegull_search::chunks::{MatchRange, SymbolMatch, locate};
use sourcegull_search::index::Cancel;
use sourcegull_search::matcher::merge;
use sourcegull_search::{FileFilter, Matcher};
use sourcegull_types::AlwaysIndex;

use crate::owners::OwnerCheck;
use crate::ranking::{self, Scorer, Scoring};
use crate::symbol_cache::SymbolCache;
use crate::trigrams::TrigramQuery;
use crate::{ChunkMatch, CompileError, chunk_matches};

/// `file:` and `lang:` filters compiled for path checks.
#[derive(Debug, Clone, Default)]
pub struct PathFilter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    langs: Vec<&'static Language>,
    exclude_langs: Vec<&'static Language>,
    /// An unknown `lang:` value: no path matches.
    never: bool,
    /// A pattern matched against the path (path search).
    pattern: Option<Matcher>,
    /// Per `^`-anchored include regexp, the literal prefixes every matching
    /// path starts with; directories that cannot hold such a path are pruned.
    prefixes: Vec<Vec<String>>,
    /// `file:has.owner()` terms at the searched commit.
    owners: Option<OwnerCheck>,
}

impl PathFilter {
    /// Compiles a planner [`FileFilter`]. Unknown languages match nothing.
    /// Predicates (`file:has.*`) are not handled here.
    pub fn compile(filter: &FileFilter, case_sensitive: bool) -> Result<PathFilter, CompileError> {
        let build = |p: &String| {
            RegexBuilder::new(p)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| CompileError::Regex {
                    pattern: p.clone(),
                    message: e.to_string(),
                })
        };
        let include = filter
            .include
            .iter()
            .map(build)
            .collect::<Result<Vec<_>, _>>()?;
        let langs: Vec<_> = filter
            .langs
            .iter()
            .filter_map(|n| sourcegull_languages::by_name(n))
            .collect();
        let never = langs.len() < filter.langs.len();
        let prefixes = filter
            .include
            .iter()
            .filter_map(|p| anchored_prefixes(p, case_sensitive))
            .collect();
        Ok(PathFilter {
            prefixes,
            include,
            exclude: filter.exclude.iter().map(build).collect::<Result<_, _>>()?,
            langs,
            exclude_langs: filter
                .exclude_langs
                .iter()
                .filter_map(|n| sourcegull_languages::by_name(n))
                .collect(),
            never,
            pattern: None,
            owners: None,
        })
    }

    /// Also requires the path to pass the owner check (`file:has.owner()`).
    pub fn with_owners(mut self, owners: OwnerCheck) -> Self {
        self.owners = Some(owners);
        self
    }

    /// Also requires the path to match `pattern`; its ranges are reported in
    /// [`FileMatch::path_ranges`].
    pub fn with_pattern(mut self, pattern: Matcher) -> Self {
        self.pattern = Some(pattern);
        self
    }

    /// Whether a path pattern is set (path search).
    pub fn has_pattern(&self) -> bool {
        self.pattern.is_some()
    }

    /// Whether a file under directory `dir` can pass the filter. False means
    /// the subtree can be skipped without reading it.
    pub fn may_contain(&self, dir: &str) -> bool {
        if self.never {
            return false;
        }
        let dir = format!("{dir}/");
        self.prefixes
            .iter()
            .all(|ps| ps.iter().any(|p| p.starts_with(&dir) || dir.starts_with(p)))
    }

    /// The path pattern's ranges in `path`: `None` if it does not match,
    /// empty when there is no path pattern.
    pub fn path_ranges(&self, path: &str) -> Option<Vec<Range<usize>>> {
        match &self.pattern {
            None => Some(Vec::new()),
            Some(p) => p.find(path.as_bytes()),
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        if self.never
            || !self.include.iter().all(|r| r.is_match(path))
            || self.exclude.iter().any(|r| r.is_match(path))
        {
            return false;
        }
        if !(self.langs.is_empty() && self.exclude_langs.is_empty()) {
            let lang = sourcegull_languages::detect(path, None);
            let is = |l: &&Language| lang.is_some_and(|x| x.id == l.id);
            if !(self.langs.iter().all(is) && !self.exclude_langs.iter().any(is)) {
                return false;
            }
        }
        self.owners.as_ref().is_none_or(|o| o.passes(path))
    }
}

/// The literal prefixes of a `^`-anchored regexp (`^src/`, `^(a|b)/x`), or
/// `None` when the pattern is unanchored or its prefixes are unbounded.
fn anchored_prefixes(pattern: &str, case_sensitive: bool) -> Option<Vec<String>> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    use regex_syntax::hir::{Hir, HirKind, Look};
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(!case_sensitive)
        .build()
        .parse(pattern)
        .ok()?;
    let HirKind::Concat(parts) = hir.kind() else {
        return None;
    };
    let (first, rest) = parts.split_first()?;
    if !matches!(first.kind(), HirKind::Look(Look::Start)) {
        return None;
    }
    let seq = Extractor::new()
        .kind(ExtractKind::Prefix)
        .extract(&Hir::concat(rest.to_vec()));
    seq.literals()?
        .iter()
        .map(|l| String::from_utf8(l.as_bytes().to_vec()).ok())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnindexedOptions {
    pub max_file_size: u64,
    /// Files searched up to [`sourcegull_types::ALWAYS_INDEX_MAX_FILE_SIZE`] whatever
    /// `max_file_size` is, as the index reads them.
    pub always_index: AlwaysIndex,
    pub max_line_len: Option<usize>,
    /// Stop after this many matching files.
    pub file_limit: usize,
    /// Threads reading and searching blobs at once.
    pub threads: usize,
    /// For scoring: whether the pattern is case-sensitive and
    /// whether to rank with BM25 (keyword queries).
    pub case_sensitive: bool,
    pub bm25: bool,
    /// Checked between batches of files; a canceled search ends early with
    /// [`SearchStats::canceled`] set.
    pub cancel: Cancel,
}

impl Default for UnindexedOptions {
    fn default() -> Self {
        Self {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            always_index: AlwaysIndex::default(),
            max_line_len: None,
            file_limit: usize::MAX,
            threads: default_threads(),
            case_sensitive: false,
            bm25: false,
            cancel: Cancel::default(),
        }
    }
}

/// One matching file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMatch {
    pub path: String,
    pub oid: ObjectId,
    /// Empty for path-only results (no content pattern, or only negations).
    pub chunks: Vec<ChunkMatch>,
    /// Byte ranges in `path` matched by the path pattern.
    pub path_ranges: Vec<Range<usize>>,
    /// Symbol search: the matching symbols.
    pub symbols: Vec<SymbolMatch>,
    /// The ranking score, without symbol definitions (unindexed
    /// search does not parse files for them); 0 without a pattern.
    pub score: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub files_considered: usize,
    pub files_searched: usize,
    pub files_skipped_large: usize,
    pub files_skipped_binary: usize,
    /// The file limit or the caller stopped the walk.
    pub limit_hit: bool,
    /// The walk stopped at [`UnindexedOptions::cancel`] (also sets `limit_hit`).
    pub canceled: bool,
}

/// First and largest batch of files searched in parallel. Batches start
/// small so the first results come quickly.
const FIRST_BATCH: usize = 16;
const MAX_BATCH: usize = 512;

/// Tree entries looked at between checks of the cancel flag.
const CANCEL_EVERY: usize = 1024;

/// What searching one file gave.
enum Outcome {
    Large,
    Binary,
    NoMatch,
    Match(Vec<ChunkMatch>, Vec<SymbolMatch>, f64),
}

/// A file that passed the path filters, waiting to be searched.
struct Candidate {
    path: String,
    oid: ObjectId,
    path_ranges: Vec<Range<usize>>,
    /// Symbol and structural search: the file's language.
    lang: Option<&'static str>,
}

/// What files are searched for.
#[derive(Clone, Copy)]
enum Mode<'m> {
    /// Content; without a matcher, every file passing the filters.
    Content(Option<&'m Matcher>),
    /// Symbol names (all symbols without a matcher), in files whose
    /// trigrams pass `prefilter`.
    Symbols {
        matcher: Option<&'m Matcher>,
        prefilter: &'m TrigramQuery,
    },
}

/// The symbols of `content` (in language `lang`) whose names match.
pub(crate) fn matching_symbols(
    symbols: impl IntoIterator<Item = (String, Option<String>, u8, Range<usize>)>,
    matcher: Option<&Matcher>,
    newlines: &[u32],
) -> Vec<SymbolMatch> {
    symbols
        .into_iter()
        .filter(|(name, ..)| matcher.is_none_or(|m| m.is_match(name.as_bytes())))
        .map(|(name, container, kind, range)| SymbolMatch {
            name,
            container,
            kind,
            range: MatchRange {
                start: locate(newlines, range.start),
                end: locate(newlines, range.end),
            },
        })
        .collect()
}

#[allow(clippy::too_many_arguments)] // The blob search receives its independent filters and cache.
fn search_blob(
    repo: &gix::Repository,
    path: &str,
    oid: ObjectId,
    lang: Option<&str>,
    mode: Mode<'_>,
    scoring: &Scoring<'_>,
    opts: &UnindexedOptions,
    symbol_cache: Option<&SymbolCache>,
) -> sourcegull_gitstore::Result<Outcome> {
    // The header gives the size without inflating the object.
    let limit = opts.always_index.max_file_size(path, opts.max_file_size);
    if repo.find_header(oid)?.size() > limit {
        return Ok(Outcome::Large);
    }
    let blob = read_blob_by_oid(repo, oid, u64::MAX)?;
    if is_binary(&blob.content) {
        return Ok(Outcome::Binary);
    }
    let content = &blob.content;
    Ok(match mode {
        Mode::Content(matcher) => {
            match matcher.and_then(|m| m.find_hits_in(content, lang, usize::MAX)) {
                Some(hits) => Outcome::Match(
                    chunk_matches(content, &merge(&hits), opts.max_line_len),
                    Vec::new(),
                    scoring.content(path, content, &hits, &[]),
                ),
                None => Outcome::NoMatch,
            }
        }
        Mode::Symbols { matcher, prefilter } => {
            if !prefilter.may_match(&sourcegull_index_format::trigrams(content)) {
                return Ok(Outcome::NoMatch);
            }
            let lang = lang.unwrap_or_default();
            let definitions = match symbol_cache {
                Some(cache) => cache.get_or_extract(oid, lang, content),
                None => std::sync::Arc::new(sourcegull_symbols::extract(lang, content)),
            };
            let symbols = definitions.iter().cloned().map(|s| {
                let r = s.range.start as usize..s.range.end as usize;
                (s.name, s.container, s.kind as u8, r)
            });
            let newlines: Vec<u32> = memchr::memchr_iter(b'\n', content)
                .map(|n| n as u32)
                .collect();
            let symbols = matching_symbols(symbols, matcher, &newlines);
            if symbols.is_empty() {
                Outcome::NoMatch
            } else {
                let score = match matcher {
                    Some(m) => ranking::symbols_score(
                        path,
                        Some(m),
                        symbols.iter().map(|s| (s.name.as_str(), s.kind)),
                    ),
                    None => 0.0,
                };
                Outcome::Match(Vec::new(), symbols, score)
            }
        }
    })
}

/// Searches every blob of `commit` that passes `paths`. With no `matcher`,
/// every file that passes the filters is a result. `emit` can stop the walk.
///
/// Subtrees that cannot hold a matching path are not read. Blobs are read
/// and searched in parallel batches; results come out in tree order.
pub fn search_commit(
    repo: &gix::Repository,
    commit: ObjectId,
    matcher: Option<&Matcher>,
    paths: &PathFilter,
    opts: &UnindexedOptions,
    emit: impl FnMut(FileMatch) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<SearchStats> {
    walk(
        repo,
        commit,
        Mode::Content(matcher),
        paths,
        opts,
        None,
        emit,
    )
}

/// Symbol search over the blobs of `commit` that pass `paths` and
/// have a language with a symbol grammar: `matcher` matches symbol names
/// (`None` keeps every symbol), and files whose trigrams cannot match
/// `prefilter` (the pattern's trigram query) are not parsed. Emits the
/// files with matching symbols.
pub fn search_commit_symbols(
    repo: &gix::Repository,
    commit: ObjectId,
    matcher: Option<&Matcher>,
    prefilter: &TrigramQuery,
    paths: &PathFilter,
    opts: &UnindexedOptions,
    emit: impl FnMut(FileMatch) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<SearchStats> {
    search_commit_symbols_cached(repo, commit, matcher, prefilter, paths, opts, None, emit)
}

/// As `search_commit_symbols`, with an optional cache shared across searches.
#[allow(clippy::too_many_arguments)] // Mirrors the public entry point with one optional cache.
pub(crate) fn search_commit_symbols_cached(
    repo: &gix::Repository,
    commit: ObjectId,
    matcher: Option<&Matcher>,
    prefilter: &TrigramQuery,
    paths: &PathFilter,
    opts: &UnindexedOptions,
    symbol_cache: Option<&SymbolCache>,
    emit: impl FnMut(FileMatch) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<SearchStats> {
    let mode = Mode::Symbols { matcher, prefilter };
    walk(repo, commit, mode, paths, opts, symbol_cache, emit)
}

fn walk(
    repo: &gix::Repository,
    commit: ObjectId,
    mode: Mode<'_>,
    paths: &PathFilter,
    opts: &UnindexedOptions,
    symbol_cache: Option<&SymbolCache>,
    mut emit: impl FnMut(FileMatch) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<SearchStats> {
    let mut stats = SearchStats::default();
    let mut found = 0usize;
    let mut err = None;
    let list_opts = ListTreeOptions {
        recursive: true,
        limit: usize::MAX,
        ..ListTreeOptions::default()
    };
    let reads_blobs = !matches!(mode, Mode::Content(None));
    let symbols = matches!(mode, Mode::Symbols { .. });
    let shared = reads_blobs.then(|| repo.clone().into_sync());
    let matcher = match mode {
        Mode::Content(m) | Mode::Symbols { matcher: m, .. } => m,
    };
    let scorer = Scorer::new(matcher, opts.case_sensitive, opts.bm25);
    // Structural templates need the file's language.
    let structural = matcher.is_some_and(Matcher::is_structural);
    let scoring = scorer.unindexed();
    let mut batch: Vec<Candidate> = Vec::new();
    let mut batch_size = FIRST_BATCH;
    let mut seen = 0usize;
    let canceled = |stats: &mut SearchStats| {
        if opts.cancel.is_canceled() {
            stats.canceled = true;
            stats.limit_hit = true;
        }
        stats.canceled
    };
    // Emits one result; Break once the limit is hit or `emit` stops.
    let mut send = |c: Candidate,
                    chunks: Vec<ChunkMatch>,
                    symbols: Vec<SymbolMatch>,
                    score: Option<f64>,
                    stats: &mut SearchStats| {
        found += 1;
        let score = score.unwrap_or_else(|| match c.path_ranges.is_empty() {
            true => 0.0,
            false => ranking::path_score(&c.path, &c.path_ranges),
        });
        let flow = emit(FileMatch {
            path: c.path,
            oid: c.oid,
            chunks,
            path_ranges: c.path_ranges,
            symbols,
            score,
        });
        if flow.is_break() || found >= opts.file_limit {
            stats.limit_hit = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    };
    type Send<'s> = dyn FnMut(
            Candidate,
            Vec<ChunkMatch>,
            Vec<SymbolMatch>,
            Option<f64>,
            &mut SearchStats,
        ) -> ControlFlow<()>
        + 's;
    let flush = |batch: &mut Vec<Candidate>,
                 stats: &mut SearchStats,
                 err: &mut Option<sourcegull_gitstore::GitError>,
                 send: &mut Send<'_>| {
        let Some(shared) = &shared else {
            return ControlFlow::Continue(());
        };
        if canceled(stats) {
            return ControlFlow::Break(());
        }
        let batch = std::mem::take(batch);
        let items: Vec<(&str, ObjectId, Option<&str>)> = batch
            .iter()
            .map(|c| (c.path.as_str(), c.oid, c.lang))
            .collect();
        let outcomes = map_ordered(shared, items, opts.threads, |r, (path, oid, lang)| {
            search_blob(r, path, oid, lang, mode, &scoring, opts, symbol_cache)
        });
        for (c, outcome) in batch.into_iter().zip(outcomes) {
            match outcome {
                Err(e) => {
                    *err = Some(e);
                    return ControlFlow::Break(());
                }
                Ok(Outcome::Large) => stats.files_skipped_large += 1,
                Ok(Outcome::Binary) => stats.files_skipped_binary += 1,
                Ok(Outcome::NoMatch) => stats.files_searched += 1,
                Ok(Outcome::Match(chunks, symbols, score)) => {
                    stats.files_searched += 1;
                    send(c, chunks, symbols, Some(score), stats)?;
                }
            }
        }
        ControlFlow::Continue(())
    };
    list_tree_pruned(
        repo,
        commit,
        &list_opts,
        |dir| paths.may_contain(&dir.path),
        |entry| {
            seen += 1;
            if seen % CANCEL_EVERY == 0 && canceled(&mut stats) {
                return ControlFlow::Break(());
            }
            if !entry.kind.is_blob() || !paths.matches(&entry.path) {
                return ControlFlow::Continue(());
            }
            // As the indexer detects it, so both backends agree.
            let lang = if symbols {
                match sourcegull_languages::detect(&entry.path, None) {
                    Some(l) if sourcegull_symbols::supported(l.id) => Some(l.id),
                    _ => return ControlFlow::Continue(()),
                }
            } else if structural {
                sourcegull_languages::detect(&entry.path, None).map(|l| l.id)
            } else {
                None
            };
            let Some(path_ranges) = paths.path_ranges(&entry.path) else {
                return ControlFlow::Continue(());
            };
            stats.files_considered += 1;
            let c = Candidate {
                path: entry.path,
                oid: entry.oid,
                path_ranges,
                lang,
            };
            if !reads_blobs {
                return send(c, Vec::new(), Vec::new(), None, &mut stats);
            }
            batch.push(c);
            if batch.len() < batch_size {
                return ControlFlow::Continue(());
            }
            batch_size = (batch_size * 2).min(MAX_BATCH);
            flush(&mut batch, &mut stats, &mut err, &mut send)
        },
    )?;
    if !stats.limit_hit && err.is_none() && !batch.is_empty() {
        let _ = flush(&mut batch, &mut stats, &mut err, &mut send);
    }
    match err {
        Some(e) => Err(e),
        None => Ok(stats),
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use sourcegull_query::PatternType;
    use sourcegull_search::{Job, Limits, PatternInfo, plan_query};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn fixture() -> (tempfile::TempDir, ObjectId) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/main.go"), "package main\n\nfunc Foo() {}\n").unwrap();
        std::fs::write(p.join("src/lib.rs"), "fn foo() {}\n").unwrap();
        std::fs::write(p.join("README.md"), "# foo\n").unwrap();
        std::fs::write(p.join("data.bin"), b"foo\0bar").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);
        let head = git(p, &["rev-parse", "HEAD"]);
        (dir, ObjectId::from_hex(head.trim().as_bytes()).unwrap())
    }

    #[test]
    fn cached_symbols_follow_blob_changes_across_revisions() {
        let (dir, old) = fixture();
        let p = dir.path();
        std::fs::write(p.join("src/main.go"), "package main\n\nfunc Bar() {}\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "rename function"]);
        let new = ObjectId::from_hex(git(p, &["rev-parse", "HEAD"]).trim().as_bytes()).unwrap();
        let repo = gix::open(p).unwrap();
        let paths = PathFilter::compile(&FileFilter::default(), false).unwrap();
        let cache = SymbolCache::new(1024 * 1024);
        let run = |commit: ObjectId, cached: bool| {
            let mut names = Vec::new();
            search_commit_symbols_cached(
                &repo,
                commit,
                None,
                &TrigramQuery::All,
                &paths,
                &UnindexedOptions::default(),
                cached.then_some(&cache),
                |file| {
                    names.extend(file.symbols.into_iter().map(|s| s.name));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
            names.sort();
            names
        };
        assert_eq!(run(old, true), run(old, false));
        assert_eq!(run(new, true), run(new, false));
        assert_eq!(run(old, true), run(old, false));
        assert!(run(old, true).contains(&"Foo".to_owned()));
        assert!(run(new, true).contains(&"Bar".to_owned()));
    }

    fn content_job(q: &str) -> (PatternInfo, FileFilter) {
        let job = plan_query(
            q,
            PatternType::Keyword,
            &Limits::default(),
            chrono::Utc::now(),
        )
        .unwrap();
        fn find(j: &Job) -> Option<(PatternInfo, FileFilter)> {
            match j {
                Job::Content { pattern, files, .. } => Some((pattern.clone(), files.clone())),
                Job::Parallel(js) => js.iter().find_map(find),
                Job::Select { child, .. }
                | Job::Limit { child, .. }
                | Job::Timeout { child, .. } => find(child),
                _ => None,
            }
        }
        find(&job).expect("content job")
    }

    fn run(q: &str) -> (Vec<String>, SearchStats) {
        let (dir, head) = fixture();
        let repo = gix::open(dir.path()).unwrap();
        let (pattern, files) = content_job(q);
        let m = Matcher::compile(&pattern).unwrap();
        let pf = PathFilter::compile(&files, pattern.case_sensitive).unwrap();
        let mut paths = Vec::new();
        let stats = search_commit(
            &repo,
            head,
            Some(&m),
            &pf,
            &UnindexedOptions::default(),
            |f| {
                paths.push(f.path);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        (paths, stats)
    }

    #[test]
    fn searches_text_files_and_skips_binary() {
        let (paths, stats) = run("foo");
        assert_eq!(paths, ["README.md", "src/lib.rs", "src/main.go"]);
        assert_eq!(stats.files_skipped_binary, 1);
    }

    #[test]
    fn path_pattern() {
        let (dir, head) = fixture();
        let repo = gix::open(dir.path()).unwrap();
        let spec = crate::PatternSpec::Atom {
            regex: "main".into(),
            negated: false,
        };
        let pf = PathFilter::default().with_pattern(Matcher::from_spec(&spec, false).unwrap());
        let mut out = Vec::new();
        search_commit(&repo, head, None, &pf, &UnindexedOptions::default(), |f| {
            out.push((f.path, f.path_ranges));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(out, [("src/main.go".to_owned(), vec![4..8])]);
    }

    #[test]
    fn file_and_lang_filters() {
        assert_eq!(run("foo file:^src/").0, ["src/lib.rs", "src/main.go"]);
        assert_eq!(run("foo -file:\\.md$").0, ["src/lib.rs", "src/main.go"]);
        assert_eq!(run("foo lang:go").0, ["src/main.go"]);
        assert_eq!(run("foo -lang:go").0, ["README.md", "src/lib.rs"]);
        assert!(run("foo lang:nosuchlang").0.is_empty());
    }

    #[test]
    fn anchored_prefixes_and_pruning() {
        let p = |re: &str| anchored_prefixes(re, true);
        assert_eq!(p("^src/"), Some(vec!["src/".to_owned()]));
        assert_eq!(p("^docs/.*\\.md$"), Some(vec!["docs/".to_owned()]));
        let mut ab = p("^(a|b)/x").unwrap();
        ab.sort();
        assert_eq!(ab, ["a/x", "b/x"]);
        assert_eq!(p("src/"), None);
        // A shared `^` is factored out of the alternation.
        assert_eq!(
            p("^src/|^lib/"),
            Some(vec!["src/".to_owned(), "lib/".to_owned()])
        );
        assert_eq!(p("^src/|lib/"), None);
        assert_eq!(anchored_prefixes("^Ab", false).map(|v| v.len()), Some(4));

        let filter = |include: &[&str]| {
            let f = FileFilter {
                include: include.iter().map(|s| (*s).to_owned()).collect(),
                ..FileFilter::default()
            };
            PathFilter::compile(&f, true).unwrap()
        };
        let f = filter(&["^src/foo/"]);
        assert!(f.may_contain("src"));
        assert!(f.may_contain("src/foo"));
        assert!(f.may_contain("src/foo/bar"));
        assert!(!f.may_contain("srcx"));
        assert!(!f.may_contain("src/bar"));
        assert!(!f.may_contain("docs"));
        // Every include must allow the directory; unanchored ones allow all.
        let f = filter(&["^src/", "\\.go$"]);
        assert!(f.may_contain("src/x"));
        assert!(!f.may_contain("lib"));
        assert!(filter(&[]).may_contain("anything"));
    }

    #[test]
    fn pruned_and_parallel_search_agree_with_sequential() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        for d in ["a", "a/deep", "b", "c"] {
            std::fs::create_dir_all(p.join(d)).unwrap();
            for i in 0..40 {
                let body = if i % 3 == 0 { "needle\n" } else { "hay\n" };
                std::fs::write(p.join(format!("{d}/f{i:02}.txt")), body).unwrap();
            }
        }
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);
        let head = ObjectId::from_hex(git(p, &["rev-parse", "HEAD"]).trim().as_bytes()).unwrap();
        let repo = gix::open(p).unwrap();
        let m = Matcher::from_spec(
            &crate::PatternSpec::Atom {
                regex: "needle".into(),
                negated: false,
            },
            false,
        )
        .unwrap();
        let search = |include: &[&str], threads: usize, limit: usize| {
            let f = FileFilter {
                include: include.iter().map(|s| (*s).to_owned()).collect(),
                ..FileFilter::default()
            };
            let pf = PathFilter::compile(&f, false).unwrap();
            let opts = UnindexedOptions {
                threads,
                file_limit: limit,
                ..UnindexedOptions::default()
            };
            let mut out = Vec::new();
            let stats = search_commit(&repo, head, Some(&m), &pf, &opts, |f| {
                out.push(f.path);
                ControlFlow::Continue(())
            })
            .unwrap();
            (out, stats)
        };
        let (seq, seq_stats) = search(&[], 1, usize::MAX);
        assert_eq!(seq.len(), 4 * 14);
        assert_eq!(seq_stats.files_searched, 160);
        assert_eq!(search(&[], 4, usize::MAX), (seq.clone(), seq_stats));
        // Pruned to a/ (a/deep included): same files as filtering the full walk.
        let (pruned, stats) = search(&["^a/"], 4, usize::MAX);
        let expected: Vec<_> = seq
            .iter()
            .filter(|p| p.starts_with("a/"))
            .cloned()
            .collect();
        assert_eq!(pruned, expected);
        assert_eq!(stats.files_considered, 80);
        // The limit stops in tree order.
        let (first, stats) = search(&[], 4, 5);
        assert_eq!(first, seq[..5]);
        assert!(stats.limit_hit);
    }
}
