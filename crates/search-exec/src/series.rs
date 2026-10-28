//! Trends counting on repo nodes.
//!
//! A series query is compiled by the worker (`sourcegull-insights`) into a [`SeriesSpec`]: the
//! parts of the query ([`SeriesLeaf`]: content, path, symbol, commit or diff counts with
//! their file and commit filters), how matches are grouped into values ([`Group`]), and the
//! `repo:has.file(…)` predicates. [`count_series`] counts it at points of one repository's
//! history:
//!
//! - The commit of each point is the newest first-parent ancestor of the head committed at or
//!   before the point's time, found by binary search over the first-parent chain read from
//!   the commit metadata log ([`point_commits`]).
//! - The oldest point is counted in full ([`SeriesCounter::count_full`]). Each later point is
//!   counted from the previous one ([`SeriesCounter::count_delta`]): a tree diff between the
//!   two commits gives the changed paths, and `count(new) = count(prev) − Σ count(old versions)
//!   + Σ count(new versions)`. Commit and diff counts follow the commit ranges in the same
//!   way: `count(new) = count(prev) + count(new ^prev) − count(prev ^new)`.
//! - Counts decompose per file, so this is exact for content, path, symbol and capture-group
//!   counts. Series with `repo:has.file(…)` predicates (whose truth depends on other files)
//!   are counted in full at every point.
//!
//! File counts are cached by blob OID ([`CountCache`]): a file version is counted once per
//! series, whichever repository or commit it appears in.

use std::hash::{Hash, Hasher};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use regex::bytes::{Regex, RegexBuilder};
use sourcegull_compute::{Command, GroupBy, MetaEnv, ValueCounts};
use sourcegull_gitstore::commitlog::CommitLog;
use sourcegull_gitstore::diff::{DiffOptions, diff_commits_where};
use sourcegull_gitstore::parallel::{default_threads, map_ordered};
use sourcegull_gitstore::read::{
    DEFAULT_MAX_FILE_SIZE, ListTreeOptions, is_binary, list_tree_pruned, read_blob_by_oid,
};
use sourcegull_gitstore::{GitError, ObjectId};
use sourcegull_rpc::ErrorKind;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search::index::Cancel;
use sourcegull_search::matcher::merge;
use sourcegull_search::{FileFilter, Matcher};
use sourcegull_search_client::{
    CommitFilterSpec, commit_filters_from_pb, commit_filters_to_pb, path_filter_from_pb,
    path_filter_to_pb, spec_from_pb, spec_to_pb,
};

use crate::commits::{CommitFilters, CommitSearchOptions, search_commits};
use crate::trigrams::TrigramQuery;
use crate::unindexed::PathFilter;
use crate::{CompileError, PatternSpec, trigram_query};

/// What a [`SeriesLeaf`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeafKind {
    /// Matches of the pattern in file contents; a file matched only through negations
    /// counts 1.
    Content,
    /// 1 per file whose path passes the filters (and matches the pattern, if any).
    Path,
    /// Symbol definitions whose names match the pattern.
    Symbol,
    /// 1 per commit matching the pattern (message) and the commit filters.
    Commit,
    /// 1 per non-merge commit whose diff matches the pattern.
    Diff,
}

impl LeafKind {
    fn is_commit(self) -> bool {
        matches!(self, LeafKind::Commit | LeafKind::Diff)
    }
}

/// One part of a series query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesLeaf {
    pub kind: LeafKind,
    /// `None` matches everything.
    pub pattern: Option<PatternSpec>,
    pub case_sensitive: bool,
    /// `file:` and `lang:` filters (predicates are not read; see `file_contents`).
    pub paths: FileFilter,
    /// `file:has.content(…)`: (negated, regexp) a file's contents must match.
    pub file_contents: Vec<(bool, String)>,
    /// Commit and diff leaves.
    pub commit_filters: CommitFilterSpec,
}

impl SeriesLeaf {
    /// A leaf of `kind` matching `pattern` everywhere.
    pub fn new(kind: LeafKind, pattern: Option<PatternSpec>) -> Self {
        Self {
            kind,
            pattern,
            case_sensitive: false,
            paths: FileFilter::default(),
            file_contents: Vec::new(),
            commit_filters: CommitFilterSpec::default(),
        }
    }
}

/// How a series groups its matches into values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// One value, the empty string: the match count (`search` series).
    None,
    /// The first capture group of the capture regexp in each match (`mapping-compute`).
    Capture,
    /// `search-compute`: the repository name, the file's language, the file's path, the
    /// commit's author, or the commit's date (committer date, `YYYY-MM-DD` in UTC).
    Repo,
    Lang,
    Path,
    Author,
    Date,
}

/// `repo:has.file(path:… content:…)` (or `has.path`, `has.content`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPredicate {
    pub negated: bool,
    pub path: Option<String>,
    pub content: Option<String>,
}

/// A compiled series query, as sent to repo nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesSpec {
    pub leaves: Vec<SeriesLeaf>,
    pub group: Group,
    /// [`Group::Capture`]: the regexp whose first capture group gives the values.
    pub capture: Option<String>,
    pub capture_case_sensitive: bool,
    pub repo_predicates: Vec<RepoPredicate>,
}

impl SeriesSpec {
    /// A `search` series of `leaves`.
    pub fn count(leaves: Vec<SeriesLeaf>) -> Self {
        Self {
            leaves,
            group: Group::None,
            capture: None,
            capture_case_sensitive: false,
            repo_predicates: Vec::new(),
        }
    }

    pub fn to_pb(&self) -> pb::SeriesSpec {
        use pb::series_leaf::Kind;
        use pb::series_spec::Group as G;
        pb::SeriesSpec {
            leaves: self
                .leaves
                .iter()
                .map(|l| pb::SeriesLeaf {
                    kind: match l.kind {
                        LeafKind::Content => Kind::Content,
                        LeafKind::Path => Kind::Path,
                        LeafKind::Symbol => Kind::Symbol,
                        LeafKind::Commit => Kind::Commit,
                        LeafKind::Diff => Kind::Diff,
                    } as i32,
                    pattern: l.pattern.as_ref().map(spec_to_pb),
                    case_sensitive: l.case_sensitive,
                    paths: Some(path_filter_to_pb(&l.paths)),
                    file_contents: l
                        .file_contents
                        .iter()
                        .map(|(negated, regex)| pb::pattern_node::Atom {
                            regex: regex.clone(),
                            negated: *negated,
                        })
                        .collect(),
                    commit_filters: Some(commit_filters_to_pb(&l.commit_filters)),
                })
                .collect(),
            group: match self.group {
                Group::None => G::None,
                Group::Capture => G::Capture,
                Group::Repo => G::Repo,
                Group::Lang => G::Lang,
                Group::Path => G::Path,
                Group::Author => G::Author,
                Group::Date => G::Date,
            } as i32,
            capture: self.capture.clone().unwrap_or_default(),
            capture_case_sensitive: self.capture_case_sensitive,
            repo_predicates: self
                .repo_predicates
                .iter()
                .map(|p| pb::SeriesRepoPredicate {
                    negated: p.negated,
                    path: p.path.clone().unwrap_or_default(),
                    content: p.content.clone().unwrap_or_default(),
                })
                .collect(),
        }
    }

    pub fn from_pb(spec: pb::SeriesSpec) -> Self {
        use pb::series_leaf::Kind;
        use pb::series_spec::Group as G;
        let some = |s: String| (!s.is_empty()).then_some(s);
        Self {
            group: match G::try_from(spec.group).unwrap_or(G::None) {
                G::None => Group::None,
                G::Capture => Group::Capture,
                G::Repo => Group::Repo,
                G::Lang => Group::Lang,
                G::Path => Group::Path,
                G::Author => Group::Author,
                G::Date => Group::Date,
            },
            leaves: spec
                .leaves
                .into_iter()
                .map(|l| SeriesLeaf {
                    kind: match Kind::try_from(l.kind).unwrap_or(Kind::Content) {
                        Kind::Content => LeafKind::Content,
                        Kind::Path => LeafKind::Path,
                        Kind::Symbol => LeafKind::Symbol,
                        Kind::Commit => LeafKind::Commit,
                        Kind::Diff => LeafKind::Diff,
                    },
                    pattern: l.pattern.as_ref().map(spec_from_pb),
                    case_sensitive: l.case_sensitive,
                    paths: path_filter_from_pb(l.paths.unwrap_or_default()),
                    file_contents: l
                        .file_contents
                        .into_iter()
                        .map(|a| (a.negated, a.regex))
                        .collect(),
                    commit_filters: commit_filters_from_pb(l.commit_filters.unwrap_or_default()),
                })
                .collect(),
            capture: some(spec.capture),
            capture_case_sensitive: spec.capture_case_sensitive,
            repo_predicates: spec
                .repo_predicates
                .into_iter()
                .map(|p| RepoPredicate {
                    negated: p.negated,
                    path: some(p.path),
                    content: some(p.content),
                })
                .collect(),
        }
    }
}

/// Counts as sent over the wire.
pub fn counts_to_pb(c: &ValueCounts) -> Vec<pb::ValueCount> {
    c.0.iter()
        .map(|(value, count)| pb::ValueCount {
            value: value.clone(),
            count: *count,
        })
        .collect()
}

pub fn counts_from_pb(c: &[pb::ValueCount]) -> ValueCounts {
    let mut out = ValueCounts::default();
    for v in c {
        bump(&mut out, &v.value, v.count);
    }
    out
}

/// Adds `n` to `value` (which may be empty: the match count of a `search` series).
fn bump(c: &mut ValueCounts, value: &str, n: u64) {
    if n > 0 {
        *c.0.entry(value.to_owned()).or_default() += n;
    }
}

/// The value of [`Group::Date`]: a committer time's UTC day (`YYYY-MM-DD`).
fn commit_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

/// Cache key: (series fingerprint, leaf, blob, language id for language-dependent leaves).
type CountKey = (u64, u16, ObjectId, &'static str);

/// Counts of file versions, by blob OID, shared by the series counted on a node (identical
/// files across repositories, such as vendored code, are counted once). Bounded
/// by size.
#[derive(Clone)]
pub struct CountCache {
    cache: moka::sync::Cache<CountKey, Arc<ValueCounts>>,
}

impl CountCache {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_: &CountKey, v: &Arc<ValueCounts>| {
                    let bytes = std::mem::size_of::<CountKey>()
                        + 64
                        + v.0.keys().map(|k| k.len() + 48).sum::<usize>();
                    u32::try_from(bytes).unwrap_or(u32::MAX)
                })
                .build(),
        }
    }
}

/// What counting a series did.
#[derive(Debug, Default)]
pub struct CountStats {
    pub full_points: u32,
    pub delta_points: u32,
    pub same_points: u32,
    pub files_counted: AtomicU64,
    pub cache_hits: AtomicU64,
}

impl CountStats {
    pub fn to_pb(&self, elapsed_ms: u64) -> pb::CountSeriesStats {
        pb::CountSeriesStats {
            full_points: self.full_points,
            delta_points: self.delta_points,
            same_points: self.same_points,
            files_counted: self.files_counted.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            elapsed_ms,
        }
    }
}

struct CompiledLeaf {
    kind: LeafKind,
    matcher: Option<Matcher>,
    /// File filters, plus the path pattern for path leaves.
    paths: PathFilter,
    file_contents: Vec<(bool, Regex)>,
    /// Symbol leaves: files whose trigrams cannot match are not parsed.
    prefilter: Option<TrigramQuery>,
    /// Structural templates need the file's language.
    structural: bool,
    commit_filters: Option<CommitFilters>,
    commit_paths: Option<PathFilter>,
}

impl CompiledLeaf {
    /// Whether counting a file needs its contents.
    fn reads_content(&self) -> bool {
        matches!(self.kind, LeafKind::Content | LeafKind::Symbol) || !self.file_contents.is_empty()
    }
}

struct CompiledPredicate {
    negated: bool,
    path: Option<Regex>,
    content: Option<Regex>,
}

/// A [`SeriesSpec`] compiled for one repository.
pub struct SeriesCounter {
    leaves: Vec<CompiledLeaf>,
    group: Group,
    capture: Option<Command>,
    predicates: Vec<CompiledPredicate>,
    repo: String,
    fingerprint: u64,
    max_file_size: u64,
    threads: usize,
    cancel: Cancel,
}

/// Tree entries looked at between checks of the cancel flag.
const CANCEL_EVERY: usize = 1024;

/// The error of a count stopped by its [`Cancel`].
fn canceled() -> GitError {
    GitError::Cancelled("count series".into())
}

fn regex(pattern: &str, case_sensitive: bool) -> Result<Regex, CompileError> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| CompileError::Regex {
            pattern: pattern.to_owned(),
            message: e.to_string(),
        })
}

/// Whether a git file mode is a blob (regular file, executable or symlink), as unindexed
/// search counts them.
fn is_blob_mode(mode: u32) -> bool {
    matches!(mode & 0o170000, 0o100000 | 0o120000)
}

impl SeriesCounter {
    /// Compiles `spec` for the repository named `repo`.
    pub fn compile(spec: &SeriesSpec, repo: &str) -> Result<Self, CompileError> {
        let mut leaves = Vec::with_capacity(spec.leaves.len());
        for l in &spec.leaves {
            let matcher = l
                .pattern
                .as_ref()
                .map(|p| Matcher::from_spec(p, l.case_sensitive))
                .transpose()?;
            let mut paths = PathFilter::compile(&l.paths, l.case_sensitive)?;
            if l.kind == LeafKind::Path
                && let Some(m) = &matcher
            {
                paths = paths.with_pattern(m.clone());
            }
            let file_contents = l
                .file_contents
                .iter()
                .map(|(neg, re)| Ok((*neg, regex(re, l.case_sensitive)?)))
                .collect::<Result<Vec<_>, CompileError>>()?;
            let commit = l.kind.is_commit();
            leaves.push(CompiledLeaf {
                kind: l.kind,
                prefilter: (l.kind == LeafKind::Symbol).then(|| {
                    l.pattern
                        .as_ref()
                        .map_or(TrigramQuery::All, |p| trigram_query(p, l.case_sensitive))
                }),
                structural: matcher.as_ref().is_some_and(Matcher::is_structural),
                commit_filters: commit
                    .then(|| CommitFilters::compile(&l.commit_filters, l.case_sensitive))
                    .transpose()?,
                commit_paths: (commit && !l.paths.is_empty())
                    .then(|| PathFilter::compile(&l.paths, l.case_sensitive))
                    .transpose()?,
                matcher,
                paths,
                file_contents,
            });
        }
        let capture = match (spec.group, &spec.capture) {
            (Group::Capture, Some(c)) => Some(
                Command::group_by(c, spec.capture_case_sensitive, GroupBy::Capture).map_err(
                    |e| CompileError::Regex {
                        pattern: c.clone(),
                        message: e.to_string(),
                    },
                )?,
            ),
            (Group::Capture, None) => {
                return Err(CompileError::Regex {
                    pattern: String::new(),
                    message: "a capture group series needs a regexp".into(),
                });
            }
            _ => None,
        };
        let predicates = spec
            .repo_predicates
            .iter()
            .map(|p| {
                Ok(CompiledPredicate {
                    negated: p.negated,
                    path: p.path.as_deref().map(|r| regex(r, false)).transpose()?,
                    content: p.content.as_deref().map(|r| regex(r, false)).transpose()?,
                })
            })
            .collect::<Result<_, CompileError>>()?;
        let mut h = std::hash::DefaultHasher::new();
        format!("{spec:?}").hash(&mut h);
        Ok(Self {
            leaves,
            group: spec.group,
            capture,
            predicates,
            repo: repo.to_owned(),
            fingerprint: h.finish(),
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            threads: default_threads(),
            cancel: Cancel::default(),
        })
    }

    /// Stops counting (with [`GitError::Cancelled`]) once `cancel` is canceled; it is
    /// checked per file, per batch of tree entries and per batch of commits.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = cancel;
        self
    }

    /// Uses `threads` threads to count files (1 runs inline).
    #[must_use]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads.max(1);
        self
    }

    /// Whether counts decompose per file and commit, so points can be counted from the
    /// previous one; false with `repo:has.file(…)` predicates.
    pub fn is_decomposable(&self) -> bool {
        self.predicates.is_empty()
    }

    fn file_leaves(&self) -> impl Iterator<Item = (usize, &CompiledLeaf)> {
        self.leaves
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.kind.is_commit())
    }

    /// Whether a file at `path` can count for some leaf (cheap: no contents read).
    fn may_count(&self, path: &str) -> bool {
        self.file_leaves().any(|(_, l)| l.paths.matches(path))
    }

    /// Adds a leaf's counts for one file, regrouped by the series' group.
    fn regroup(&self, out: &mut ValueCounts, counts: &ValueCounts, path: &str) {
        match self.group {
            Group::None => bump(out, "", counts.total()),
            Group::Capture => out.add(counts),
            Group::Repo => bump(out, &self.repo, counts.total()),
            Group::Path => bump(out, path, counts.total()),
            Group::Lang => {
                if let Some(l) = sourcegull_languages::detect(path, None) {
                    bump(out, l.name, counts.total());
                }
            }
            // Files have no author or date; compilation keeps these groups to commit leaves.
            Group::Author | Group::Date => {}
        }
    }

    /// The counts of one file version, grouped.
    fn count_file(
        &self,
        repo: &gix::Repository,
        path: &str,
        oid: ObjectId,
        cache: Option<&CountCache>,
        stats: &CountStats,
    ) -> sourcegull_gitstore::Result<ValueCounts> {
        if self.cancel.is_canceled() {
            return Err(canceled());
        }
        let mut out = ValueCounts::default();
        let mut blob: Option<Option<Vec<u8>>> = None;
        let mut detected = None;
        for (i, leaf) in self.file_leaves() {
            if !leaf.paths.matches(path) {
                continue;
            }
            if leaf.kind == LeafKind::Path && leaf.paths.path_ranges(path).is_none() {
                continue;
            }
            let lang: &'static str = if leaf.kind == LeafKind::Symbol {
                match *detected.get_or_insert_with(|| sourcegull_languages::detect(path, None)) {
                    Some(l) if sourcegull_symbols::supported(l.id) => l.id,
                    _ => continue,
                }
            } else if leaf.structural {
                detected
                    .get_or_insert_with(|| sourcegull_languages::detect(path, None))
                    .map_or("", |l| l.id)
            } else {
                ""
            };
            let counts = if leaf.reads_content() {
                stats.files_counted.fetch_add(1, Ordering::Relaxed);
                let key = (self.fingerprint, i as u16, oid, lang);
                if let Some(c) = cache.and_then(|c| c.cache.get(&key)) {
                    stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                    c
                } else {
                    if blob.is_none() {
                        blob = Some(self.read(repo, oid)?);
                    }
                    let c = Arc::new(match blob.as_ref().and_then(Option::as_deref) {
                        Some(content) => self.count_content(leaf, content, lang),
                        None => ValueCounts::default(),
                    });
                    if let Some(cache) = cache {
                        cache.cache.insert(key, c.clone());
                    }
                    c
                }
            } else {
                let mut c = ValueCounts::default();
                bump(&mut c, "", 1);
                Arc::new(c)
            };
            self.regroup(&mut out, &counts, path);
        }
        Ok(out)
    }

    /// A blob's contents, or `None` when it is too large or binary (not searched, as in
    /// unindexed search).
    fn read(
        &self,
        repo: &gix::Repository,
        oid: ObjectId,
    ) -> sourcegull_gitstore::Result<Option<Vec<u8>>> {
        if repo.find_header(oid)?.size() > self.max_file_size {
            return Ok(None);
        }
        let blob = read_blob_by_oid(repo, oid, u64::MAX)?;
        Ok((!is_binary(&blob.content)).then_some(blob.content))
    }

    /// What `leaf` counts in `content` (language `lang`), before grouping: the match count
    /// under the empty value, or the capture values.
    fn count_content(&self, leaf: &CompiledLeaf, content: &[u8], lang: &str) -> ValueCounts {
        let mut out = ValueCounts::default();
        if !leaf
            .file_contents
            .iter()
            .all(|(negated, re)| re.is_match(content) != *negated)
        {
            return out;
        }
        let lang = (!lang.is_empty()).then_some(lang);
        match leaf.kind {
            LeafKind::Content => {
                let Some(hits) = leaf
                    .matcher
                    .as_ref()
                    .map(|m| m.find_hits_in(content, lang, usize::MAX))
                else {
                    bump(&mut out, "", 1);
                    return out;
                };
                let Some(hits) = hits else {
                    return out;
                };
                match &self.capture {
                    Some(cmd) => {
                        out = cmd.count_file(&MetaEnv::default(), &String::from_utf8_lossy(content))
                    }
                    None => bump(&mut out, "", merge(&hits).len().max(1) as u64),
                }
            }
            LeafKind::Symbol => {
                if leaf
                    .prefilter
                    .as_ref()
                    .is_some_and(|p| !p.may_match(&sourcegull_index_format::trigrams(content)))
                {
                    return out;
                }
                let n = sourcegull_symbols::extract(lang.unwrap_or_default(), content)
                    .iter()
                    .filter(|s| {
                        leaf.matcher
                            .as_ref()
                            .is_none_or(|m| m.is_match(s.name.as_bytes()))
                    })
                    .count();
                bump(&mut out, "", n as u64);
            }
            // Path leaves read contents only for `file:has.content`, which passed.
            LeafKind::Path => bump(&mut out, "", 1),
            LeafKind::Commit | LeafKind::Diff => {}
        }
        out
    }

    /// Counts many file versions in parallel and sums them.
    fn count_files(
        &self,
        repo: &gix::Repository,
        files: Vec<(String, ObjectId)>,
        cache: Option<&CountCache>,
        stats: &CountStats,
    ) -> sourcegull_gitstore::Result<ValueCounts> {
        let mut total = ValueCounts::default();
        if files.is_empty() {
            return Ok(total);
        }
        let shared = repo.clone().into_sync();
        let results = map_ordered(&shared, files, self.threads, |r, (path, oid)| {
            self.count_file(r, &path, oid, cache, stats)
        });
        for r in results {
            total.add(&r?);
        }
        Ok(total)
    }

    /// The matching commits reachable from `start` but not from `hide`, per commit leaf.
    fn count_commits(
        &self,
        repo: &gix::Repository,
        start: ObjectId,
        hide: Option<ObjectId>,
        log: Option<&Arc<CommitLog>>,
    ) -> sourcegull_gitstore::Result<ValueCounts> {
        let mut out = ValueCounts::default();
        for leaf in self.leaves.iter().filter(|l| l.kind.is_commit()) {
            let Some(filters) = &leaf.commit_filters else {
                continue;
            };
            let opts = CommitSearchOptions {
                paths: leaf.commit_paths.clone(),
                diff: leaf.kind == LeafKind::Diff,
                limit: usize::MAX,
                threads: self.threads,
                log: if hide.is_none() { log.cloned() } else { None },
                diff_cache: None,
                hide: hide.into_iter().collect(),
                cancel: self.cancel.clone(),
            };
            let end = search_commits(repo, start, filters, leaf.matcher.as_ref(), &opts, |m| {
                match self.group {
                    Group::Author => bump(&mut out, &m.commit.author.name, 1),
                    Group::Date => bump(&mut out, &commit_date(m.commit.committer.time), 1),
                    Group::Repo => bump(&mut out, &self.repo, 1),
                    _ => bump(&mut out, "", 1),
                }
                ControlFlow::Continue(())
            })?;
            if end.canceled {
                return Err(canceled());
            }
        }
        Ok(out)
    }

    fn has_commit_leaves(&self) -> bool {
        self.leaves.iter().any(|l| l.kind.is_commit())
    }

    /// Whether the `repo:has.file(…)` predicates hold at `commit`.
    fn predicates_hold(
        &self,
        repo: &gix::Repository,
        commit: ObjectId,
    ) -> sourcegull_gitstore::Result<bool> {
        if self.predicates.is_empty() {
            return Ok(true);
        }
        let mut hit = vec![false; self.predicates.len()];
        let mut err = None;
        let mut seen = 0usize;
        let opts = ListTreeOptions {
            recursive: true,
            limit: usize::MAX,
            ..ListTreeOptions::default()
        };
        list_tree_pruned(
            repo,
            commit,
            &opts,
            |_| true,
            |entry| {
                seen += 1;
                if seen % CANCEL_EVERY == 0 && self.cancel.is_canceled() {
                    err = Some(canceled());
                    return ControlFlow::Break(());
                }
                if !entry.kind.is_blob() {
                    return ControlFlow::Continue(());
                }
                let mut content: Option<Option<Vec<u8>>> = None;
                for (p, h) in self.predicates.iter().zip(hit.iter_mut()) {
                    if *h
                        || p.path
                            .as_ref()
                            .is_some_and(|re| !re.is_match(entry.path.as_bytes()))
                    {
                        continue;
                    }
                    match &p.content {
                        None => *h = true,
                        Some(re) => {
                            if content.is_none() {
                                match self.read(repo, entry.oid) {
                                    Ok(c) => content = Some(c),
                                    Err(e) => {
                                        err = Some(e);
                                        return ControlFlow::Break(());
                                    }
                                }
                            }
                            *h = content
                                .as_ref()
                                .and_then(Option::as_deref)
                                .is_some_and(|c| re.is_match(c));
                        }
                    }
                }
                if hit.iter().all(|h| *h) {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )?;
        if let Some(e) = err {
            return Err(e);
        }
        Ok(self
            .predicates
            .iter()
            .zip(&hit)
            .all(|(p, h)| *h != p.negated))
    }

    /// Counts the series at `commit` from scratch.
    pub fn count_full(
        &self,
        repo: &gix::Repository,
        commit: ObjectId,
        log: Option<&Arc<CommitLog>>,
        cache: Option<&CountCache>,
        stats: &CountStats,
    ) -> sourcegull_gitstore::Result<ValueCounts> {
        if !self.predicates_hold(repo, commit)? {
            return Ok(ValueCounts::default());
        }
        let mut files = Vec::new();
        let mut seen = 0usize;
        let mut stopped = false;
        if self.file_leaves().next().is_some() {
            let opts = ListTreeOptions {
                recursive: true,
                limit: usize::MAX,
                ..ListTreeOptions::default()
            };
            list_tree_pruned(
                repo,
                commit,
                &opts,
                |dir| {
                    self.file_leaves()
                        .any(|(_, l)| l.paths.may_contain(&dir.path))
                },
                |entry| {
                    seen += 1;
                    if seen % CANCEL_EVERY == 0 && self.cancel.is_canceled() {
                        stopped = true;
                        return ControlFlow::Break(());
                    }
                    if entry.kind.is_blob() && self.may_count(&entry.path) {
                        files.push((entry.path, entry.oid));
                    }
                    ControlFlow::Continue(())
                },
            )?;
        }
        if stopped {
            return Err(canceled());
        }
        let mut total = self.count_files(repo, files, cache, stats)?;
        if self.has_commit_leaves() {
            total.add(&self.count_commits(repo, commit, None, log)?);
        }
        Ok(total)
    }

    /// Counts the series at `to` from its counts `prev` at `from`: the files the tree diff
    /// names are counted at both commits, and commit leaves count the two commit ranges.
    pub fn count_delta(
        &self,
        repo: &gix::Repository,
        from: ObjectId,
        to: ObjectId,
        prev: &ValueCounts,
        cache: Option<&CountCache>,
        stats: &CountStats,
    ) -> sourcegull_gitstore::Result<ValueCounts> {
        let (mut old, mut new) = (Vec::new(), Vec::new());
        if self.file_leaves().next().is_some() {
            let opts = DiffOptions {
                headers_only: true,
                detect_renames: false,
                max_files: usize::MAX,
                ..DiffOptions::default()
            };
            diff_commits_where(
                repo,
                Some(from),
                to,
                &opts,
                |f| {
                    let side = |p: &Option<String>, m: u32| {
                        p.as_deref()
                            .is_some_and(|p| is_blob_mode(m) && self.may_count(p))
                    };
                    side(&f.old_path, f.old_mode) || side(&f.new_path, f.new_mode)
                },
                |f| {
                    if let (Some(p), Some(oid)) = (f.old_path, f.old_oid)
                        && is_blob_mode(f.old_mode)
                        && self.may_count(&p)
                    {
                        old.push((p, oid));
                    }
                    if let (Some(p), Some(oid)) = (f.new_path, f.new_oid)
                        && is_blob_mode(f.new_mode)
                        && self.may_count(&p)
                    {
                        new.push((p, oid));
                    }
                    ControlFlow::Continue(())
                },
            )?;
        }
        let mut out = prev.clone();
        out.subtract(&self.count_files(repo, old, cache, stats)?);
        if self.has_commit_leaves() {
            out.subtract(&self.count_commits(repo, from, Some(to), None)?);
        }
        out.add(&self.count_files(repo, new, cache, stats)?);
        if self.has_commit_leaves() {
            out.add(&self.count_commits(repo, to, Some(from), None)?);
        }
        Ok(out)
    }
}

/// The first-parent chain from `head`, newest first, with committer times: from the commit
/// metadata log while it has the commits, then from git.
fn first_parent_chain(
    repo: &gix::Repository,
    log: Option<&CommitLog>,
    head: ObjectId,
) -> sourcegull_gitstore::Result<Vec<(ObjectId, i64)>> {
    let mut chain = Vec::new();
    let mut next = Some(head);
    if let Some(log) = log
        && let Some(mut pos) = log.lookup(&head)
    {
        next = None;
        loop {
            let c = log.get(pos);
            chain.push((c.oid(), c.committer_time()));
            match c.parents().next() {
                Some(Some(p)) => pos = p,
                // A parent the log lacks: continue in git.
                Some(None) => {
                    let commit = sourcegull_gitstore::read::find_commit(repo, c.oid())?;
                    next = commit.parent_ids().next().map(|p| p.detach());
                    break;
                }
                None => break,
            }
        }
    }
    while let Some(oid) = next {
        let info = sourcegull_gitstore::read::commit_info(repo, oid)?;
        chain.push((oid, info.committer.time));
        next = info.parents.first().copied();
    }
    Ok(chain)
}

/// For each time (seconds since the epoch), the newest first-parent ancestor of `head`
/// (inclusive) committed at or before it; `None` when there is none. A binary search over the
/// chain's running minimum of committer times, so out-of-order dates give the same answer as
/// a scan from the head.
pub fn point_commits(
    repo: &gix::Repository,
    log: Option<&CommitLog>,
    head: ObjectId,
    times: &[i64],
) -> sourcegull_gitstore::Result<Vec<Option<ObjectId>>> {
    let chain = first_parent_chain(repo, log, head)?;
    let mut min = i64::MAX;
    let mins: Vec<i64> = chain
        .iter()
        .map(|(_, t)| {
            min = min.min(*t);
            min
        })
        .collect();
    Ok(times
        .iter()
        .map(|t| {
            let i = mins.partition_point(|m| m > t);
            chain.get(i).map(|(oid, _)| *oid)
        })
        .collect())
}

/// One counted point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesPoint {
    pub time: i64,
    pub commit: Option<ObjectId>,
    pub counts: ValueCounts,
}

/// Which points to count and how.
#[derive(Debug, Clone, Default)]
pub struct SeriesRun {
    /// Point times, ascending.
    pub times: Vec<i64>,
    /// Count every point at the head (recording) instead of the commit nearest its time.
    pub at_head: bool,
    /// The previous point, to count incrementally from.
    pub base: Option<(ObjectId, ValueCounts)>,
    /// Count every point in full.
    pub full: bool,
}

/// Counts `counter` at the points of `run` in the history of `head` (see the module docs).
pub fn count_series(
    repo: &gix::Repository,
    log: Option<&Arc<CommitLog>>,
    head: ObjectId,
    counter: &SeriesCounter,
    run: &SeriesRun,
    cache: Option<&CountCache>,
) -> sourcegull_gitstore::Result<(Vec<SeriesPoint>, CountStats)> {
    let mut stats = CountStats::default();
    let commits = if run.at_head {
        vec![Some(head); run.times.len()]
    } else {
        point_commits(repo, log.map(|l| &**l), head, &run.times)?
    };
    // A base whose commit is gone (force push, then gc) is not used.
    let mut prev: Option<(ObjectId, ValueCounts)> = run
        .base
        .clone()
        .filter(|(c, _)| repo.find_commit(*c).is_ok());
    let mut points = Vec::with_capacity(run.times.len());
    for (&time, commit) in run.times.iter().zip(commits) {
        if counter.cancel.is_canceled() {
            return Err(canceled());
        }
        let counts = match commit {
            None => ValueCounts::default(),
            Some(c) => match &prev {
                Some((p, pc)) if *p == c && counter.is_decomposable() && !run.full => {
                    stats.same_points += 1;
                    pc.clone()
                }
                Some((p, pc)) if counter.is_decomposable() && !run.full => {
                    stats.delta_points += 1;
                    counter.count_delta(repo, *p, c, pc, cache, &stats)?
                }
                _ => {
                    stats.full_points += 1;
                    counter.count_full(repo, c, log, cache, &stats)?
                }
            },
        };
        prev = commit.map(|c| (c, counts.clone()));
        points.push(SeriesPoint {
            time,
            commit,
            counts,
        });
    }
    Ok((points, stats))
}

/// Runs a `CountSeries` request against an opened repository (the gRPC handler's body).
pub(crate) fn count_series_request(
    repo: &gix::Repository,
    log: Option<&Arc<CommitLog>>,
    req: pb::CountSeriesRequest,
    cache: &CountCache,
    cancel: Cancel,
) -> Result<pb::CountSeriesResponse, tonic::Status> {
    let started = Instant::now();
    let name = req.repo.map(|r| r.name).unwrap_or_default();
    let spec = SeriesSpec::from_pb(req.spec.unwrap_or_default());
    let counter = SeriesCounter::compile(&spec, &name)
        .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?
        .with_cancel(cancel);
    let base = match req.base {
        Some(b) if !b.commit_oid.is_empty() => Some((
            sourcegull_gitstore::read::parse_oid(&b.commit_oid)?,
            counts_from_pb(&b.counts),
        )),
        _ => None,
    };
    let empty = |times: &[i64]| pb::CountSeriesResponse {
        points: times
            .iter()
            .map(|t| pb::SeriesPoint {
                time: *t,
                ..Default::default()
            })
            .collect(),
        stats: Some(CountStats::default().to_pb(0)),
    };
    let head = match sourcegull_gitstore::read::resolve_revision(repo, &req.rev) {
        Ok(h) => h,
        // An empty repository (or a branch not created yet) has nothing to count.
        Err(GitError::RevisionNotFound(_)) if req.rev.is_empty() => {
            return Ok(empty(&req.times));
        }
        Err(e) => return Err(e.into()),
    };
    let run = SeriesRun {
        times: req.times,
        at_head: req.at_head,
        base,
        full: req.full,
    };
    let (points, stats) =
        count_series(repo, log, head, &counter, &run, Some(cache)).map_err(|e| match e {
            GitError::Cancelled(_) => {
                ErrorKind::Timeout.status("counting was canceled or reached the call's deadline")
            }
            e => e.into(),
        })?;
    Ok(pb::CountSeriesResponse {
        points: points
            .into_iter()
            .map(|p| pb::SeriesPoint {
                time: p.time,
                commit_oid: p.commit.map(|c| c.to_string()).unwrap_or_default(),
                counts: counts_to_pb(&p.counts),
            })
            .collect(),
        stats: Some(stats.to_pb(started.elapsed().as_millis() as u64)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command as Cmd;

    /// Epoch of the test commits' dates.
    const B: i64 = 1_600_000_000;

    /// Runs git in `dir` with the commit date `B + time`.
    fn git(dir: &Path, time: i64, args: &[&str]) -> String {
        let date = format!("{} +0000", B + time);
        let out = Cmd::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(dir: &Path, time: i64, files: &[(&str, Option<&str>)]) -> ObjectId {
        let dir_path = dir;
        for (path, content) in files {
            let p = dir.join(path);
            match content {
                Some(c) => {
                    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                    std::fs::write(p, c).unwrap();
                }
                None => {
                    let _ = std::fs::remove_file(&p);
                    // Drop directories left empty, so a file may take their place.
                    let mut dir = p.parent();
                    while let Some(d) = dir.filter(|d| *d != dir_path) {
                        if std::fs::remove_dir(d).is_err() {
                            break;
                        }
                        dir = d.parent();
                    }
                }
            }
        }
        git(dir, time, &["add", "-A"]);
        git(dir, time, &["commit", "-q", "--allow-empty", "-m", "c"]);
        ObjectId::from_hex(git(dir, time, &["rev-parse", "HEAD"]).as_bytes()).unwrap()
    }

    fn content(pattern: &str) -> SeriesLeaf {
        SeriesLeaf::new(
            LeafKind::Content,
            Some(PatternSpec::Atom {
                regex: pattern.into(),
                negated: false,
            }),
        )
    }

    fn total(c: &ValueCounts) -> u64 {
        c.total()
    }

    #[test]
    fn point_commits_take_the_newest_commit_at_or_before_each_time() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, 0, &["init", "-q"]);
        let a = commit(d, 1000, &[("a", Some("x"))]);
        // Committed "earlier" than its parent (a rebased commit with an old date).
        let b = commit(d, 900, &[("a", Some("y"))]);
        let c = commit(d, 3000, &[("a", Some("z"))]);
        let repo = gix::open(d).unwrap();
        let times = [500, 950, 1000, 2999, 3000, 9999].map(|t| B + t);
        let got = point_commits(&repo, None, c, &times).unwrap();
        // At 950 only `b` (900) qualifies on a scan from the head, but its parent `a` (1000)
        // is newer than 950, so `b` is still the newest commit at or before 950.
        assert_eq!(got, vec![None, Some(b), Some(b), Some(b), Some(c), Some(c)]);
        let _ = a;
    }

    #[test]
    fn delta_counts_equal_full_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, 0, &["init", "-q"]);
        let c1 = commit(
            d,
            100,
            &[
                ("src/a.go", Some("TODO one\nTODO two\n")),
                ("src/b.go", Some("nothing\n")),
                ("docs/x.md", Some("TODO doc\n")),
            ],
        );
        let c2 = commit(
            d,
            200,
            &[
                ("src/a.go", Some("TODO one\n")),
                ("src/b.go", None),
                ("src/c/d.go", Some("TODO todo TODO\n")),
            ],
        );
        // A file replaced by a directory, and a rename.
        let c3 = commit(
            d,
            300,
            &[
                ("docs/x.md", None),
                ("docs/y.md", Some("TODO doc\n")),
                ("src/c/d.go", None),
                ("src/c", Some("TODO file\n")),
            ],
        );
        let repo = gix::open(d).unwrap();
        let mut leaf = content("todo");
        leaf.paths.include = vec![r"\.go$".into(), "^src/".into()];
        for spec in [
            SeriesSpec::count(vec![content("todo")]),
            SeriesSpec::count(vec![leaf]),
            SeriesSpec {
                group: Group::Path,
                ..SeriesSpec::count(vec![content("todo")])
            },
        ] {
            let counter = SeriesCounter::compile(&spec, "r").unwrap();
            let run = SeriesRun {
                times: vec![B + 100, B + 200, B + 300],
                ..SeriesRun::default()
            };
            let (inc, stats) = count_series(&repo, None, c3, &counter, &run, None).unwrap();
            assert_eq!((stats.full_points, stats.delta_points), (1, 2));
            let full = SeriesRun { full: true, ..run };
            let (want, _) = count_series(&repo, None, c3, &counter, &full, None).unwrap();
            assert_eq!(inc, want, "{spec:?}");
            assert_eq!(
                inc.iter().map(|p| p.commit).collect::<Vec<_>>(),
                vec![Some(c1), Some(c2), Some(c3)]
            );
        }
        let counter =
            SeriesCounter::compile(&SeriesSpec::count(vec![content("todo")]), "r").unwrap();
        let stats = CountStats::default();
        assert_eq!(
            total(&counter.count_full(&repo, c1, None, None, &stats).unwrap()),
            3
        );
        assert_eq!(
            total(&counter.count_full(&repo, c2, None, None, &stats).unwrap()),
            5
        );
        assert_eq!(
            total(&counter.count_full(&repo, c3, None, None, &stats).unwrap()),
            3
        );
    }

    #[test]
    fn capture_symbols_commits_and_predicates() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, 0, &["init", "-q"]);
        let c1 = commit(d, 100, &[("a.go", Some("func Foo() {}\n// TODO(ann) x\n"))]);
        let c2 = commit(
            d,
            200,
            &[(
                "b.go",
                Some("func Bar() {}\nfunc FooBar() {}\n// TODO(bob) TODO(ann)\n"),
            )],
        );
        let repo = gix::open(d).unwrap();
        let stats = CountStats::default();
        let count = |spec: SeriesSpec, at: ObjectId| {
            SeriesCounter::compile(&spec, "r")
                .unwrap()
                .count_full(&repo, at, None, None, &stats)
                .unwrap()
        };
        let capture = SeriesSpec {
            group: Group::Capture,
            capture: Some(r"TODO\((\w+)\)".into()),
            ..SeriesSpec::count(vec![content(r"TODO\(")])
        };
        let c = count(capture.clone(), c2);
        assert_eq!((c.get("ann"), c.get("bob")), (2, 1));
        let symbols = SeriesSpec::count(vec![SeriesLeaf::new(
            LeafKind::Symbol,
            Some(PatternSpec::Atom {
                regex: "foo".into(),
                negated: false,
            }),
        )]);
        assert_eq!(count(symbols, c2).total(), 2);
        let commits = SeriesSpec {
            group: Group::Author,
            ..SeriesSpec::count(vec![SeriesLeaf::new(LeafKind::Commit, None)])
        };
        assert_eq!(count(commits.clone(), c2).get("t"), 2);
        let counter = SeriesCounter::compile(&commits, "r").unwrap();
        let delta = counter
            .count_delta(&repo, c1, c2, &count(commits.clone(), c1), None, &stats)
            .unwrap();
        assert_eq!(delta.get("t"), 2);
        let dates = SeriesSpec {
            group: Group::Date,
            ..commits.clone()
        };
        assert_eq!(count(dates.clone(), c2).get(&commit_date(B + 200)), 2);
        assert_eq!(SeriesSpec::from_pb(dates.to_pb()), dates);
        assert_eq!(commit_date(86_400 * 365 + 5), "1971-01-01");
        // repo:has.path(b\.go): nothing before b.go exists.
        let pred = SeriesSpec {
            repo_predicates: vec![RepoPredicate {
                negated: false,
                path: Some(r"b\.go".into()),
                content: None,
            }],
            ..SeriesSpec::count(vec![content("func")])
        };
        assert_eq!(count(pred.clone(), c1).total(), 0);
        assert_eq!(count(pred.clone(), c2).total(), 3);
        assert!(
            !SeriesCounter::compile(&pred, "r")
                .unwrap()
                .is_decomposable()
        );
        // The spec survives the wire.
        assert_eq!(SeriesSpec::from_pb(capture.to_pb()), capture);
        assert_eq!(SeriesSpec::from_pb(pred.to_pb()), pred);
    }

    #[test]
    fn identical_blobs_are_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, 0, &["init", "-q"]);
        let c1 = commit(
            d,
            100,
            &[("a/x.txt", Some("TODO\n")), ("b/x.txt", Some("TODO\n"))],
        );
        let repo = gix::open(d).unwrap();
        let cache = CountCache::new(1 << 20);
        let counter = SeriesCounter::compile(&SeriesSpec::count(vec![content("todo")]), "r")
            .unwrap()
            .with_threads(1);
        let stats = CountStats::default();
        let c = counter
            .count_full(&repo, c1, None, Some(&cache), &stats)
            .unwrap();
        assert_eq!(c.total(), 2);
        assert_eq!(stats.files_counted.load(Ordering::Relaxed), 2);
        assert_eq!(stats.cache_hits.load(Ordering::Relaxed), 1);
    }
}
