//! Commit and diff search (`type:commit`, `type:diff`) on a repo node.
//!
//! Walks history from a commit, newest first, and keeps commits that pass the
//! `author:`/`committer:`/`message:`/`before:`/`after:` filters and whose
//! message matches the pattern. Diff search diffs the commits that pass the
//! filters, in parallel batches, and matches the pattern against added and
//! removed lines.
//!
//! When the repo's commit metadata log ([`CommitLog`]) has the start commit,
//! the walk and the filters run over the log, without reading git objects;
//! otherwise (a log not built yet or behind the refs) history is walked in
//! git. Both give the same commits in the same order. Diff search keeps the
//! rendered diffs of recent commits in a [`DiffCache`].

use std::ops::{ControlFlow, Range};
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use sourcegull_gitstore::ObjectId;
use sourcegull_gitstore::commitlog::CommitLog;
use sourcegull_gitstore::diff::{DiffOptions, diff_commits_where};
use sourcegull_gitstore::parallel::{default_threads, map_ordered};
use sourcegull_gitstore::read::{CommitInfo, LogOptions, log_each};
use sourcegull_search::Matcher;
use sourcegull_search::index::Cancel;
use sourcegull_search_client::CommitFilterSpec;

use crate::{CompileError, PathFilter};

/// Compiled commit filters. Regexps are case-insensitive unless the query
/// says `case:yes`.
#[derive(Debug, Clone, Default)]
pub struct CommitFilters {
    pub authors: Vec<Regex>,
    pub exclude_authors: Vec<Regex>,
    pub committers: Vec<Regex>,
    pub exclude_committers: Vec<Regex>,
    pub messages: Vec<Regex>,
    pub exclude_messages: Vec<Regex>,
    /// Committer time bounds, seconds since the epoch.
    pub before: Option<i64>,
    pub after: Option<i64>,
}

impl CommitFilters {
    pub fn compile(spec: &CommitFilterSpec, case_sensitive: bool) -> Result<Self, CompileError> {
        let all = |ps: &[String]| {
            ps.iter()
                .map(|p| {
                    RegexBuilder::new(p)
                        .case_insensitive(!case_sensitive)
                        .build()
                        .map_err(|e| CompileError::Regex {
                            pattern: p.clone(),
                            message: e.to_string(),
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(CommitFilters {
            authors: all(&spec.authors)?,
            exclude_authors: all(&spec.exclude_authors)?,
            committers: all(&spec.committers)?,
            exclude_committers: all(&spec.exclude_committers)?,
            messages: all(&spec.messages)?,
            exclude_messages: all(&spec.exclude_messages)?,
            before: spec.before,
            after: spec.after,
        })
    }

    pub fn matches(&self, c: &CommitInfo) -> bool {
        self.matches_parts(
            || format!("{} <{}>", c.author.name, c.author.email),
            || format!("{} <{}>", c.committer.name, c.committer.email),
            c.committer.time,
            &c.message,
        )
    }

    /// [`matches`](Self::matches) for a commit in the log.
    pub fn matches_log(&self, c: &sourcegull_gitstore::commitlog::LogCommit<'_>) -> bool {
        self.matches_parts(
            || c.author().ident,
            || c.committer().ident,
            c.committer_time(),
            c.message(),
        )
    }

    /// Times first, then the identities (`name <email>`, built only when a
    /// filter needs them), then the message.
    fn matches_parts<A: AsRef<str>, C: AsRef<str>>(
        &self,
        author: impl FnOnce() -> A,
        committer: impl FnOnce() -> C,
        committer_time: i64,
        message: &str,
    ) -> bool {
        let pass = |s: &str, inc: &[Regex], exc: &[Regex]| {
            inc.iter().all(|r| r.is_match(s)) && !exc.iter().any(|r| r.is_match(s))
        };
        let needs = |inc: &[Regex], exc: &[Regex]| !inc.is_empty() || !exc.is_empty();
        self.before.is_none_or(|t| committer_time < t)
            && self.after.is_none_or(|t| committer_time > t)
            && (!needs(&self.authors, &self.exclude_authors)
                || pass(author().as_ref(), &self.authors, &self.exclude_authors))
            && (!needs(&self.committers, &self.exclude_committers)
                || pass(
                    committer().as_ref(),
                    &self.committers,
                    &self.exclude_committers,
                ))
            && pass(message, &self.messages, &self.exclude_messages)
    }
}

/// A matching commit and the pattern's ranges in its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMatch {
    pub commit: CommitInfo,
    /// Commit search: the pattern's ranges in the message.
    pub message_ranges: Vec<Range<usize>>,
    /// Diff search: the changed files with matching hunks.
    pub diff: Vec<DiffFileMatch>,
}

/// A changed file in a diff match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFileMatch {
    /// `None` when the file was added.
    pub old_path: Option<String>,
    /// `None` when the file was deleted.
    pub new_path: Option<String>,
    pub hunks: Vec<DiffHunkMatch>,
}

/// A hunk in a diff match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunkMatch {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Lines prefixed with ` `, `+`, `-` or `\`, each newline-terminated.
    pub body: String,
    /// Byte ranges of pattern matches in `body`, sorted.
    pub ranges: Vec<Range<usize>>,
    /// The text after the `@@` header: git's function-context heading, then
    /// `... +N` when N more matches were cut from the hunk.
    pub section: String,
}

/// What a commit search looks at besides the metadata filters.
#[derive(Debug, Clone)]
pub struct CommitSearchOptions {
    /// Keeps commits that change a file passing this filter; for diff search,
    /// only those files are searched.
    pub paths: Option<PathFilter>,
    /// Diff search: the pattern matches added and removed lines, and merge
    /// commits are skipped (they have no diff of their own, as in `git log -p`).
    pub diff: bool,
    /// Stop after this many matches.
    pub limit: usize,
    /// Threads diffing commits at once.
    pub threads: usize,
    /// The repo's commit metadata log, used when it has the start commit.
    pub log: Option<Arc<CommitLog>>,
    /// Diff search: rendered diffs of this repo's commits.
    pub diff_cache: Option<(DiffCache, sourcegull_types::RepoId)>,
    /// Leave out these commits and their ancestors (`start ^hide…`): a commit range, as code
    /// monitors search. The walk then runs in git, not over the metadata log.
    pub hide: Vec<ObjectId>,
    /// Checked between batches of commits; a canceled search ends early with
    /// [`CommitSearchEnd::canceled`] set.
    pub cancel: Cancel,
}

/// How a commit search ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitSearchEnd {
    /// The limit, `emit` or the cancel flag stopped the walk.
    pub limit_hit: bool,
    /// The walk stopped at [`CommitSearchOptions::cancel`] (also sets `limit_hit`).
    pub canceled: bool,
}

impl Default for CommitSearchOptions {
    fn default() -> Self {
        Self {
            paths: None,
            diff: false,
            limit: usize::MAX,
            threads: default_threads(),
            log: None,
            diff_cache: None,
            hide: Vec::new(),
            cancel: Cancel::default(),
        }
    }
}

/// Rendered diffs of recent commits (all files, before pattern matching),
/// shared by the searches of a repo node and bounded by size.
#[derive(Clone)]
pub struct DiffCache {
    cache: moka::sync::Cache<(sourcegull_types::RepoId, ObjectId), Arc<CachedDiff>>,
}

struct CachedDiff {
    files: Vec<DiffFileMatch>,
    /// The diff stopped at `max_files`, so it cannot answer for a file filter.
    truncated: bool,
}

impl std::fmt::Debug for DiffCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffCache")
            .field("entries", &self.cache.entry_count())
            .field("bytes", &self.cache.weighted_size())
            .finish()
    }
}

impl DiffCache {
    /// A cache of up to about `max_bytes` of hunks and paths.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_, d: &Arc<CachedDiff>| {
                    let bytes: usize = d
                        .files
                        .iter()
                        .map(|f| {
                            64 + f.old_path.as_ref().map_or(0, String::len)
                                + f.new_path.as_ref().map_or(0, String::len)
                                + f.hunks
                                    .iter()
                                    .map(|h| 64 + h.body.len() + h.section.len())
                                    .sum::<usize>()
                        })
                        .sum();
                    u32::try_from(bytes).unwrap_or(u32::MAX)
                })
                .build(),
        }
    }
}

/// Context lines around changes in diff matches.
const DIFF_CONTEXT: u32 = 3;

/// Hunk bytes kept per diff match; later hunks are dropped so one huge
/// commit cannot flood the stream.
const MAX_DIFF_PREVIEW: usize = 256 * 1024;

/// Diff search skips line diffs of blobs larger than this.
const MAX_DIFF_BLOB: u64 = 1 << 20;

/// First and largest batch of commits diffed in parallel. Batches start
/// small so the first results come quickly.
const FIRST_BATCH: usize = 8;
const MAX_BATCH: usize = 256;

/// Commits walked between checks of the cancel flag where each commit is cheap
/// (metadata only).
const CANCEL_EVERY: usize = 1024;

/// Walks history from `start` and emits matching commits, newest first, until
/// `opts.limit` matches, `emit` breaks or `opts.cancel` is canceled.
pub fn search_commits(
    repo: &gix::Repository,
    start: ObjectId,
    filters: &CommitFilters,
    pattern: Option<&Matcher>,
    opts: &CommitSearchOptions,
    mut emit: impl FnMut(CommitMatch) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<CommitSearchEnd> {
    let log = opts.log.as_deref();
    let mut found = 0usize;
    let mut stopped = false;
    let end = |stopped: bool, canceled: bool| CommitSearchEnd {
        limit_hit: stopped || canceled,
        canceled,
    };
    // Emits one match; Break when the walk should stop.
    let mut send = |m: CommitMatch, stopped: &mut bool| {
        found += 1;
        if emit(m).is_break() || found >= opts.limit {
            *stopped = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    };
    if !opts.diff {
        let mut err = None;
        let mut canceled = false;
        let walk_canceled = candidates(
            repo,
            log,
            start,
            &opts.hide,
            filters,
            false,
            pattern,
            &opts.cancel,
            |c, message_ranges| {
                if let Some(paths) = &opts.paths {
                    // Each commit is diffed here: check the flag for every one.
                    if opts.cancel.is_canceled() {
                        canceled = true;
                        return ControlFlow::Break(());
                    }
                    match touches(repo, c.oid, paths) {
                        Ok(true) => {}
                        Ok(false) => return ControlFlow::Continue(()),
                        Err(e) => {
                            err = Some(e);
                            return ControlFlow::Break(());
                        }
                    }
                }
                let m = CommitMatch {
                    commit: c,
                    message_ranges,
                    diff: Vec::new(),
                };
                send(m, &mut stopped)
            },
        )?;
        return err.map_or(Ok(end(stopped, canceled || walk_canceled)), Err);
    }

    let shared = repo.clone().into_sync();
    let mut batch = Vec::with_capacity(FIRST_BATCH);
    let mut batch_size = FIRST_BATCH;
    let mut err = None;
    let mut canceled = false;
    // Diffs a batch in parallel and emits its matches in walk order.
    let mut flush = |batch: &mut Vec<CommitInfo>, stopped: &mut bool| {
        if opts.cancel.is_canceled() {
            canceled = true;
            return ControlFlow::Break(());
        }
        let results = diff_batch(&shared, std::mem::take(batch), pattern, opts);
        for r in results {
            match r {
                Ok(Some(m)) => send(m, stopped)?,
                Ok(None) => {}
                Err(e) => {
                    err = Some(e);
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    };
    let walk_canceled = candidates(
        repo,
        log,
        start,
        &opts.hide,
        filters,
        true,
        None,
        &opts.cancel,
        |c, _| {
            batch.push(c);
            if batch.len() < batch_size {
                return ControlFlow::Continue(());
            }
            batch_size = (batch_size * 2).min(MAX_BATCH);
            flush(&mut batch, &mut stopped)
        },
    )?;
    if !stopped && !walk_canceled && !batch.is_empty() {
        let _ = flush(&mut batch, &mut stopped);
    }
    err.map_or(Ok(end(stopped, canceled || walk_canceled)), Err)
}

/// Calls `f` with the commits from `start`, newest first, that pass
/// `filters`, are not merges when `skip_merges`, and whose message `pattern`
/// matches, with the pattern's ranges in the message. Commits reachable from
/// `hide` are left out. Uses `log` when it has `start` and nothing is hidden,
/// else walks git. Returns whether `cancel` stopped the walk.
#[allow(clippy::too_many_arguments)]
fn candidates(
    repo: &gix::Repository,
    log: Option<&CommitLog>,
    start: ObjectId,
    hide: &[ObjectId],
    filters: &CommitFilters,
    skip_merges: bool,
    pattern: Option<&Matcher>,
    cancel: &Cancel,
    mut f: impl FnMut(CommitInfo, Vec<Range<usize>>) -> ControlFlow<()>,
) -> sourcegull_gitstore::Result<bool> {
    let mut walked = 0usize;
    let mut canceled = false;
    let mut check = || {
        canceled = walked % CANCEL_EVERY == 0 && cancel.is_canceled();
        walked += 1;
        canceled
    };
    let message_ranges = |message: &str| match pattern {
        None => Some(Vec::new()),
        Some(m) => m.find(message.as_bytes()),
    };
    let log = log.filter(|_| hide.is_empty());
    if let Some((log, pos)) = log.and_then(|l| Some((l, l.lookup(&start)?))) {
        for c in log.walk(pos) {
            if check() {
                break;
            }
            if (skip_merges && c.is_merge()) || !filters.matches_log(&c) {
                continue;
            }
            let Some(ranges) = message_ranges(c.message()) else {
                continue;
            };
            if f(c.to_info(), ranges).is_break() {
                break;
            }
        }
        return Ok(canceled);
    }
    let log_opts = LogOptions {
        limit: usize::MAX,
        hide: hide.to_vec(),
        ..LogOptions::default()
    };
    log_each(repo, start, &log_opts, |c| {
        if check() {
            return ControlFlow::Break(());
        }
        if (skip_merges && c.parents.len() > 1) || !filters.matches(&c) {
            return ControlFlow::Continue(());
        }
        match message_ranges(&c.message) {
            Some(ranges) => f(c, ranges),
            None => ControlFlow::Continue(()),
        }
    })?;
    Ok(canceled)
}

/// Whether `commit` changes a file passing `paths` (against its first parent).
fn touches(
    repo: &gix::Repository,
    commit: ObjectId,
    paths: &PathFilter,
) -> sourcegull_gitstore::Result<bool> {
    let opts = DiffOptions {
        headers_only: true,
        detect_renames: false,
        ..DiffOptions::default()
    };
    let mut hit = false;
    diff_commits_where(
        repo,
        None,
        commit,
        &opts,
        |f| passes(f.old_path.as_deref(), f.new_path.as_deref(), paths),
        |_| {
            hit = true;
            ControlFlow::Break(())
        },
    )?;
    Ok(hit)
}

fn passes(old_path: Option<&str>, new_path: Option<&str>, paths: &PathFilter) -> bool {
    old_path.is_some_and(|p| paths.matches(p)) || new_path.is_some_and(|p| paths.matches(p))
}

type DiffResult = sourcegull_gitstore::Result<Option<CommitMatch>>;

/// Runs [`diff_match`] over `commits` on up to `opts.threads` threads,
/// keeping the input order.
fn diff_batch(
    repo: &gix::ThreadSafeRepository,
    commits: Vec<CommitInfo>,
    pattern: Option<&Matcher>,
    opts: &CommitSearchOptions,
) -> Vec<DiffResult> {
    map_ordered(repo, commits, opts.threads, |r, c| {
        diff_match(r, c, pattern, opts.paths.as_ref(), opts.diff_cache.as_ref())
    })
}

/// A changed line in the matched text: where it starts in the text, and
/// where its content starts in its hunk's body.
struct Segment {
    text_start: usize,
    len: usize,
    file: usize,
    hunk: usize,
    body_start: usize,
}

/// Diffs `commit` against its first parent, restricted to `paths`.
fn diff_files(
    repo: &gix::Repository,
    commit: ObjectId,
    paths: Option<&PathFilter>,
) -> sourcegull_gitstore::Result<CachedDiff> {
    let opts = DiffOptions {
        context_lines: DIFF_CONTEXT,
        max_blob_bytes: MAX_DIFF_BLOB,
        max_files: 1_000,
        ..DiffOptions::default()
    };
    let mut files: Vec<DiffFileMatch> = Vec::new();
    let truncated = diff_commits_where(
        repo,
        None,
        commit,
        &opts,
        |f| paths.is_none_or(|p| passes(f.old_path.as_deref(), f.new_path.as_deref(), p)),
        |f| {
            files.push(DiffFileMatch {
                old_path: f.old_path,
                new_path: f.new_path,
                hunks: f
                    .hunks
                    .into_iter()
                    .map(|h| DiffHunkMatch {
                        old_start: h.old_start,
                        old_lines: h.old_lines,
                        new_start: h.new_start,
                        new_lines: h.new_lines,
                        body: h.body,
                        ranges: Vec::new(),
                        section: h.section,
                    })
                    .collect(),
            });
            ControlFlow::Continue(())
        },
    )?;
    Ok(CachedDiff { files, truncated })
}

/// The diff of `commit` from `cache`, computing and caching it on a miss.
/// With a file filter a miss is not cached: the filtered diff is cheaper to
/// compute than the whole one.
fn cached_diff(
    repo: &gix::Repository,
    commit: ObjectId,
    paths: Option<&PathFilter>,
    cache: Option<&(DiffCache, sourcegull_types::RepoId)>,
) -> sourcegull_gitstore::Result<Arc<CachedDiff>> {
    let Some((cache, repo_id)) = cache else {
        return Ok(Arc::new(diff_files(repo, commit, paths)?));
    };
    let key = (*repo_id, commit);
    if let Some(d) = cache.cache.get(&key)
        && (paths.is_none() || !d.truncated)
    {
        return Ok(d);
    }
    let d = Arc::new(diff_files(repo, commit, paths)?);
    if paths.is_none() {
        cache.cache.insert(key, d.clone());
    }
    Ok(d)
}

/// Diffs `commit` against its first parent and matches `pattern` against the
/// added and removed lines. The whole commit is one document for the pattern
/// tree, so `a AND b` may match in different files.
fn diff_match(
    repo: &gix::Repository,
    commit: CommitInfo,
    pattern: Option<&Matcher>,
    paths: Option<&PathFilter>,
    cache: Option<&(DiffCache, sourcegull_types::RepoId)>,
) -> DiffResult {
    let diff = cached_diff(repo, commit.oid, paths, cache)?;
    let kept: Vec<&DiffFileMatch> = diff
        .files
        .iter()
        .filter(|f| paths.is_none_or(|p| passes(f.old_path.as_deref(), f.new_path.as_deref(), p)))
        .collect();
    if kept.is_empty() && paths.is_some() {
        return Ok(None);
    }
    let mut files: Vec<DiffFileMatch>;
    if let Some(m) = pattern {
        let (text, segments) = changed_lines(kept.iter().copied());
        let Some(ranges) = m.find(text.as_bytes()) else {
            return Ok(None);
        };
        files = kept.into_iter().cloned().collect();
        assign_ranges(&mut files, &segments, &ranges);
    } else {
        files = kept.into_iter().cloned().collect();
    }
    // Only the matching lines with context.
    let mut files = crate::diff_preview::preview(files);
    let mut budget = MAX_DIFF_PREVIEW;
    for f in &mut files {
        let keep = f
            .hunks
            .iter()
            .take_while(|h| {
                let fits = h.body.len() <= budget;
                budget = budget.saturating_sub(h.body.len());
                fits
            })
            .count();
        f.hunks.truncate(keep);
    }
    Ok(Some(CommitMatch {
        commit,
        message_ranges: Vec::new(),
        diff: files,
    }))
}

/// The added and removed lines of `files` without their `+`/`-`, one per
/// line, and where each came from.
fn changed_lines<'a>(files: impl IntoIterator<Item = &'a DiffFileMatch>) -> (String, Vec<Segment>) {
    let mut text = String::new();
    let mut segments = Vec::new();
    for (fi, f) in files.into_iter().enumerate() {
        for (hi, h) in f.hunks.iter().enumerate() {
            let mut at = 0;
            for line in h.body.split_inclusive('\n') {
                if line.starts_with(['+', '-']) {
                    let content = line[1..].strip_suffix('\n').unwrap_or(&line[1..]);
                    segments.push(Segment {
                        text_start: text.len(),
                        len: content.len(),
                        file: fi,
                        hunk: hi,
                        body_start: at + 1,
                    });
                    text.push_str(content);
                    text.push('\n');
                }
                at += line.len();
            }
        }
    }
    (text, segments)
}

/// Maps ranges in the [`changed_lines`] text to ranges in hunk bodies,
/// splitting ranges that span lines and dropping the joining newlines.
fn assign_ranges(files: &mut [DiffFileMatch], segments: &[Segment], ranges: &[Range<usize>]) {
    for r in ranges {
        let first = segments.partition_point(|s| s.text_start + s.len < r.start);
        for s in &segments[first..] {
            if s.text_start >= r.end {
                break;
            }
            let start = r.start.max(s.text_start);
            let end = r.end.min(s.text_start + s.len);
            if start >= end {
                continue;
            }
            let b = s.body_start + (start - s.text_start);
            files[s.file].hunks[s.hunk]
                .ranges
                .push(b..b + (end - start));
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::PatternSpec;
    use sourcegull_search::FileFilter;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new("git")
            .args(args)
            .envs(env.iter().copied())
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

    fn commit(dir: &Path, msg: &str, who: &str, date: &str) {
        let email = format!("{who}@example.com");
        let env = [
            ("GIT_AUTHOR_NAME", who),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_COMMITTER_NAME", who),
            ("GIT_COMMITTER_EMAIL", email.as_str()),
            ("GIT_AUTHOR_DATE", date),
            ("GIT_COMMITTER_DATE", date),
        ];
        git(dir, &["commit", "-q", "--allow-empty", "-m", msg], &env);
    }

    fn run(filters: CommitFilterSpec, pattern: Option<&str>) -> Vec<(String, Vec<Range<usize>>)> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"], &[]);
        commit(p, "fix parser bug", "alice", "2024-01-01T00:00:00Z");
        commit(p, "add feature", "bob", "2024-02-01T00:00:00Z");
        commit(p, "fix docs", "bob", "2024-03-01T00:00:00Z");
        let head = git(p, &["rev-parse", "HEAD"], &[]);
        let repo = gix::open(p).unwrap();
        let log = build_log(&repo);
        let f = CommitFilters::compile(&filters, false).unwrap();
        let m = pattern.map(|r| {
            Matcher::from_spec(
                &PatternSpec::Atom {
                    regex: r.into(),
                    negated: false,
                },
                false,
            )
            .unwrap()
        });
        let all: Vec<_> = [None, Some(log)]
            .into_iter()
            .map(|log| {
                let mut out = Vec::new();
                let opts = CommitSearchOptions {
                    log,
                    ..CommitSearchOptions::default()
                };
                search_commits(
                    &repo,
                    ObjectId::from_hex(head.trim().as_bytes()).unwrap(),
                    &f,
                    m.as_ref(),
                    &opts,
                    |c| {
                        out.push((c.commit.subject().to_owned(), c.message_ranges));
                        ControlFlow::Continue(())
                    },
                )
                .unwrap();
                out
            })
            .collect();
        assert_eq!(all[0], all[1], "git walk and commit log differ");
        all[0].clone()
    }

    fn build_log(repo: &gix::Repository) -> Arc<CommitLog> {
        sourcegull_gitstore::commitlog::update(repo, None).unwrap();
        let path = sourcegull_gitstore::commitlog::log_path(repo.git_dir());
        Arc::new(CommitLog::open(&path).unwrap().unwrap())
    }

    #[test]
    fn pattern_and_filters() {
        let subjects = |v: Vec<(String, Vec<Range<usize>>)>| -> Vec<String> {
            v.into_iter().map(|(s, _)| s).collect()
        };
        assert_eq!(
            run(CommitFilterSpec::default(), Some("FIX")),
            [
                ("fix docs".to_owned(), vec![0..3]),
                ("fix parser bug".to_owned(), vec![0..3])
            ]
        );
        let bob = CommitFilterSpec {
            authors: vec!["bob".into()],
            ..Default::default()
        };
        assert_eq!(subjects(run(bob, Some("fix"))), ["fix docs"]);
        let window = CommitFilterSpec {
            after: Some(1_704_153_600),  // 2024-01-02
            before: Some(1_709_164_800), // 2024-02-29
            ..Default::default()
        };
        assert_eq!(subjects(run(window, None)), ["add feature"]);
        let msg = CommitFilterSpec {
            exclude_messages: vec!["docs".into()],
            ..Default::default()
        };
        assert_eq!(subjects(run(msg, None)), ["add feature", "fix parser bug"]);
    }

    fn matcher(regex: &str) -> Matcher {
        Matcher::from_spec(
            &PatternSpec::Atom {
                regex: regex.into(),
                negated: false,
            },
            false,
        )
        .unwrap()
    }

    /// A repo with three commits changing a.rs and b.go, and a merge.
    fn diff_repo() -> (tempfile::TempDir, ObjectId) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"], &[]);
        std::fs::write(p.join("a.rs"), "fn main() {}\n").unwrap();
        git(p, &["add", "."], &[]);
        commit(p, "add a", "alice", "2024-01-01T00:00:00Z");
        std::fs::write(p.join("a.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        std::fs::write(p.join("b.go"), "package b\n// hello there\n").unwrap();
        git(p, &["add", "."], &[]);
        commit(p, "say hello", "bob", "2024-02-01T00:00:00Z");
        git(p, &["checkout", "-q", "-b", "side"], &[]);
        std::fs::write(p.join("c.txt"), "side hello\n").unwrap();
        git(p, &["add", "."], &[]);
        commit(p, "side", "bob", "2024-02-02T00:00:00Z");
        git(p, &["checkout", "-q", "main"], &[]);
        std::fs::write(p.join("b.go"), "package b\n").unwrap();
        git(p, &["add", "."], &[]);
        commit(p, "drop comment", "alice", "2024-03-01T00:00:00Z");
        let env = [
            ("GIT_AUTHOR_NAME", "m"),
            ("GIT_AUTHOR_EMAIL", "m@example.com"),
            ("GIT_COMMITTER_NAME", "m"),
            ("GIT_COMMITTER_EMAIL", "m@example.com"),
            ("GIT_COMMITTER_DATE", "2024-04-01T00:00:00Z"),
            ("GIT_AUTHOR_DATE", "2024-04-01T00:00:00Z"),
        ];
        git(
            p,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
            &env,
        );
        let head = git(p, &["rev-parse", "HEAD"], &[]);
        (dir, ObjectId::from_hex(head.trim().as_bytes()).unwrap())
    }

    fn diffs(
        pattern: Option<&str>,
        paths: Option<FileFilter>,
        threads: usize,
    ) -> Vec<(String, Vec<DiffFileMatch>)> {
        let (dir, head) = diff_repo();
        let repo = gix::open(dir.path()).unwrap();
        let log = build_log(&repo);
        let cache = DiffCache::new(1 << 20);
        let m = pattern.map(matcher);
        // Git walk; log; log with the cache cold, warm, and warm without the
        // file filter (cached diffs are whole, then filtered).
        let modes = [(false, false), (true, false), (true, true), (true, true)];
        let all: Vec<_> = modes
            .into_iter()
            .map(|(use_log, use_cache)| {
                let opts = CommitSearchOptions {
                    paths: paths
                        .clone()
                        .map(|f| PathFilter::compile(&f, false).unwrap()),
                    diff: true,
                    threads,
                    log: use_log.then(|| log.clone()),
                    diff_cache: use_cache.then(|| (cache.clone(), sourcegull_types::RepoId(1))),
                    ..CommitSearchOptions::default()
                };
                let mut out = Vec::new();
                search_commits(
                    &repo,
                    head,
                    &CommitFilters::default(),
                    m.as_ref(),
                    &opts,
                    |c| {
                        out.push((c.commit.subject().to_owned(), c.diff));
                        ControlFlow::Continue(())
                    },
                )
                .unwrap();
                out
            })
            .collect();
        for other in &all[1..] {
            assert_eq!(&all[0], other, "modes differ");
        }
        all[0].clone()
    }

    #[test]
    fn cached_whole_diffs_serve_filtered_searches() {
        let (dir, head) = diff_repo();
        let repo = gix::open(dir.path()).unwrap();
        let cache = DiffCache::new(1 << 20);
        let run = |paths: Option<FileFilter>| {
            let opts = CommitSearchOptions {
                paths: paths.map(|f| PathFilter::compile(&f, false).unwrap()),
                diff: true,
                diff_cache: Some((cache.clone(), sourcegull_types::RepoId(1))),
                ..CommitSearchOptions::default()
            };
            let mut out = Vec::new();
            search_commits(&repo, head, &CommitFilters::default(), None, &opts, |c| {
                out.push((c.commit.subject().to_owned(), c.diff));
                ControlFlow::Continue(())
            })
            .unwrap();
            out
        };
        let rs = FileFilter {
            include: vec!["\\.rs$".into()],
            ..FileFilter::default()
        };
        let cold = run(Some(rs.clone()));
        assert_eq!(cache.cache.entry_count(), 0);
        run(None);
        cache.cache.run_pending_tasks();
        assert_eq!(cache.cache.entry_count(), 4);
        assert_eq!(run(Some(rs)), cold);
    }

    #[test]
    fn diff_matches_changed_lines_only() {
        for threads in [1, 4] {
            let out = diffs(Some("hello"), None, threads);
            let subjects: Vec<_> = out.iter().map(|(s, _)| s.as_str()).collect();
            // The merge is skipped; "drop comment" removes a hello line.
            assert_eq!(subjects, ["drop comment", "side", "say hello"]);
            let (_, files) = &out[2];
            assert_eq!(files.len(), 2);
            assert_eq!(files[0].new_path.as_deref(), Some("a.rs"));
            let h = &files[0].hunks[0];
            // The matching line and one line of context on each side.
            assert_eq!(h.body, "+fn main() {\n+    hello();\n+}\n");
            assert_eq!(
                (h.old_start, h.old_lines, h.new_start, h.new_lines),
                (2, 0, 1, 3)
            );
            let r = h.ranges[0].clone();
            assert_eq!(&h.body[r], "hello");
            assert_eq!(files[1].old_path, None);
            assert_eq!(files[1].new_path.as_deref(), Some("b.go"));
            let (_, removed) = &out[0];
            let h = &removed[0].hunks[0];
            assert_eq!(&h.body[h.ranges[0].clone()], "hello");
            assert!(h.body.contains("-// hello there\n"));
        }
    }

    #[test]
    fn diff_file_filter_and_no_pattern() {
        let rs = FileFilter {
            include: vec!["\\.rs$".into()],
            ..FileFilter::default()
        };
        let out = diffs(None, Some(rs), 2);
        let subjects: Vec<_> = out.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(subjects, ["say hello", "add a"]);
        assert!(
            out.iter()
                .all(|(_, f)| f.len() == 1 && f[0].hunks.len() == 1)
        );
        // No match anywhere in the changed lines.
        assert!(
            diffs(Some("package"), Some(FileFilter::default()), 2)
                .iter()
                .all(|(s, _)| s != "drop comment")
        );
    }

    #[test]
    fn hidden_commits_bound_a_range() {
        // `head ^<say hello>`: what a push after "say hello" added (watches),
        // the same with and without the metadata log (which a range bypasses).
        let (dir, head) = diff_repo();
        let repo = gix::open(dir.path()).unwrap();
        let old = git(dir.path(), &["rev-parse", "main~2"], &[]);
        let old = ObjectId::from_hex(old.trim().as_bytes()).unwrap();
        let log = build_log(&repo);
        for (log, diff) in [(None, false), (Some(log.clone()), false), (Some(log), true)] {
            let opts = CommitSearchOptions {
                hide: vec![old],
                log,
                diff,
                ..CommitSearchOptions::default()
            };
            let mut out = Vec::new();
            search_commits(&repo, head, &CommitFilters::default(), None, &opts, |c| {
                out.push(c.commit.subject().to_owned());
                ControlFlow::Continue(())
            })
            .unwrap();
            if diff {
                // Merges have no diff of their own.
                assert_eq!(out, ["drop comment", "side"]);
            } else {
                assert_eq!(out, ["merge side", "drop comment", "side"]);
            }
        }
        // Hiding the head itself leaves nothing.
        let opts = CommitSearchOptions {
            hide: vec![head],
            ..CommitSearchOptions::default()
        };
        let mut n = 0;
        search_commits(&repo, head, &CommitFilters::default(), None, &opts, |_| {
            n += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn canceled_commit_search_stops_early() {
        let (dir, head) = diff_repo();
        let repo = gix::open(dir.path()).unwrap();
        let expired = Cancel::new().with_deadline(Some(std::time::Instant::now()));
        let flagged = Cancel::new();
        flagged.cancel();
        for diff in [false, true] {
            let run = |cancel: Cancel| {
                let opts = CommitSearchOptions {
                    diff,
                    cancel,
                    ..CommitSearchOptions::default()
                };
                let mut n = 0;
                let end =
                    search_commits(&repo, head, &CommitFilters::default(), None, &opts, |_| {
                        n += 1;
                        ControlFlow::Continue(())
                    })
                    .unwrap();
                (n, end)
            };
            let (n, end) = run(Cancel::new());
            assert!(n > 0, "diff: {diff}");
            assert_eq!(end, CommitSearchEnd::default(), "diff: {diff}");
            for cancel in [expired.clone(), flagged.clone()] {
                let (n, end) = run(cancel);
                assert_eq!(n, 0, "diff: {diff}");
                assert!(end.canceled && end.limit_hit, "diff: {diff}");
            }
        }
    }

    #[test]
    fn commit_search_with_file_filter() {
        let (dir, head) = diff_repo();
        let repo = gix::open(dir.path()).unwrap();
        let go = FileFilter {
            langs: vec!["go".into()],
            ..FileFilter::default()
        };
        let opts = CommitSearchOptions {
            paths: Some(PathFilter::compile(&go, false).unwrap()),
            ..CommitSearchOptions::default()
        };
        let mut out = Vec::new();
        search_commits(&repo, head, &CommitFilters::default(), None, &opts, |c| {
            out.push(c.commit.subject().to_owned());
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(out, ["drop comment", "say hello"]);
    }

    #[test]
    fn ranges_split_across_lines() {
        let mut files = vec![DiffFileMatch {
            old_path: None,
            new_path: Some("x".into()),
            hunks: vec![DiffHunkMatch {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 2,
                body: " ctx\n+ab\n+cd\n".into(),
                ranges: Vec::new(),
                section: String::new(),
            }],
        }];
        let (text, segs) = changed_lines(&files);
        assert_eq!(text, "ab\ncd\n");
        // "b\nc" spans both lines.
        assign_ranges(&mut files, &segs, &[1..4]);
        let h = &files[0].hunks[0];
        let parts: Vec<_> = h.ranges.iter().map(|r| &h.body[r.clone()]).collect();
        assert_eq!(parts, ["b", "c"]);
    }
}
