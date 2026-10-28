//! Runs a content, path, commit or diff search across repositories from
//! `web`.
//!
//! With `index:yes` (the default) content and path searches go to the index
//! first: one `SearchIndexed` call per node and batch of repositories, with
//! the default branch searched as `HEAD`. Branches the index does not hold
//! (other revisions, repositories not indexed yet, nodes without an index)
//! fall back to unindexed search, which resolves each revision and searches
//! it on the owning node; `index:only` drops them and `index:no` skips the
//! index. Results stream out as [`Event::Matches`] up to the result limit.
//! Failures become per-repo skip reasons in the final progress.
//!
//! Ranking: file results carry the node's score plus the
//! repository's share ([`ranking::repo_score`]). For the first
//! [`RANK_WINDOW`] they are held and only the best `limit` kept; then they
//! go out best first, and later ones stream in arrival order. Nodes hold
//! their own matches for
//! [`FLUSH_WALL_TIME`](sourcegull_search::index::FLUSH_WALL_TIME) and send them
//! best first, so the window closes early once every indexed call has sent
//! its first files (or ended) and no unindexed search is left.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::tonic::Status;
use sourcegull_rpc::{ErrorKind, RepoClients};
use sourcegull_search::index::DEFAULT_BRANCH;
use sourcegull_search::ranking;
use sourcegull_search::stream::{
    self, ChunkMatch, Event, Match, Progress, Severity, Skipped, SkippedReason,
};
use sourcegull_search::{FileFilter, PatternSpec, RepoRevs, Selector, YesNoOnly};
use sourcegull_types::{RepoId, SymbolKind};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::owners::OwnerSearch;
use crate::wire::{CommitFilterSpec, commit_filters_to_pb, path_filter_to_pb, spec_to_pb};

/// What to search in each repository.
#[derive(Debug, Clone)]
pub struct ContentSearch {
    /// `None` lists the files that pass `paths`.
    pub pattern: Option<PatternSpec>,
    /// Path search: a pattern the path must match. Results are path matches.
    pub path_pattern: Option<PatternSpec>,
    pub case_sensitive: bool,
    pub paths: FileFilter,
    /// Maximum results in total.
    pub limit: usize,
    pub max_line_len: u32,
    /// Repositories searched at once.
    pub concurrency: usize,
    /// Commit search instead of file search: `pattern` matches messages
    /// (or changed lines with `diff`), `paths` keeps commits changing a
    /// matching file.
    pub commits: Option<CommitFilterSpec>,
    /// Diff search (with `commits`).
    pub diff: bool,
    /// Whether content and path searches use the index (`index:`).
    pub index: YesNoOnly,
    /// Symbol search: `pattern` matches symbol names; results are symbol
    /// matches.
    pub symbols: bool,
    /// Rank with BM25 (keyword queries) instead of match quality.
    pub bm25: bool,
    /// `select:`, shared by every part of one query so projections are
    /// deduplicated across them.
    pub select: Option<Arc<Mutex<Selector>>>,
    /// `select:commit.diff.added` (`+`) or `removed` (`-`): diff matches
    /// keep only such lines that matched.
    pub diff_lines: Option<char>,
    /// `file:has.owner()` terms and `select:file.owners`, evaluated on the nodes.
    /// When it reports owners, file results become owner results ([`owner_matches`]).
    pub owners: Option<Arc<OwnerSearch>>,
    /// When the search times out. Calls to the nodes carry it, so a node stops
    /// its part at that time even when the call is not canceled.
    pub deadline: Option<Instant>,
}

impl ContentSearch {
    /// Whether results are ranked: file results of a search with a pattern.
    fn ranked(&self) -> bool {
        self.commits.is_none() && (self.pattern.is_some() || self.path_pattern.is_some())
    }
}

/// Repositories per `SearchIndexed` call, at most. A node verifies a call's
/// repositories in parallel on its search pool and ranks them together, so
/// one call per node is best: more calls verify up to `limit` files each
/// (most thrown away), and share the node's workers per call rather than
/// per query. The bound keeps a request under gRPC's 4 MB message limit.
pub(crate) const INDEXED_BATCH: usize = 10_000;

/// When, after a ranked search starts, the nodes send the best matches they
/// hold (their flush wall time, [`sourcegull_search::index::FLUSH_WALL_TIME`] for
/// one call on its own), so that the first page reaches the client within
/// 100 ms. A call that starts later holds for what is left of it, but
/// at least [`MIN_FLUSH`].
pub const FIRST_PAGE: Duration = Duration::from_millis(75);

/// The shortest flush wall time a ranked call is sent with.
pub const MIN_FLUSH: Duration = Duration::from_millis(10);

/// How long ranked results are held to be sent best first, at most: the
/// window closes earlier once every indexed call has sent its first files
/// (they flush at [`FIRST_PAGE`]) and no unindexed search is left.
pub const RANK_WINDOW: Duration = Duration::from_millis(90);

/// Ranked results held during the [`RANK_WINDOW`].
#[derive(Default)]
struct Held {
    matches: Vec<(f64, usize, Match)>,
    /// Arrival counter, to keep equal scores in arrival order.
    seq: usize,
    /// Matches were dropped for better ones.
    dropped: bool,
}

impl Held {
    /// Keeps the best `limit`, best first.
    fn trim(&mut self, limit: usize) {
        self.matches
            .sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        if self.matches.len() > limit {
            self.matches.truncate(limit);
            self.dropped = true;
        }
    }
}

struct Shared {
    search: ContentSearch,
    /// When the search started ([`FIRST_PAGE`] counts from it).
    started: Instant,
    clients: Arc<RepoClients>,
    tx: mpsc::Sender<Event>,
    sent: Mutex<usize>,
    /// `Some` while the rank window is open.
    held: Mutex<Option<Held>>,
    /// Indexed calls that have not sent files or ended, plus repositories
    /// queued for or in unindexed search; the rank window closes at zero.
    pending: AtomicUsize,
    settled: tokio::sync::Notify,
    /// Sends in progress: matches counted in `sent` but not all in `tx` yet.
    sending: AtomicUsize,
}

/// Counts a send in [`Shared::sending`] while it lives.
struct Sending<'a>(&'a AtomicUsize);

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    /// Counts work that may still send results before the rank window can
    /// close.
    fn hold(&self, n: usize) {
        self.pending.fetch_add(n, Ordering::SeqCst);
    }

    /// Counts off held work; the last one wakes the window.
    fn settle(&self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.settled.notify_one();
        }
    }

    /// Holds scored matches while the rank window is open, else sends
    /// them; false once the limit is reached or the receiver is gone.
    async fn send_scored(&self, matches: Vec<(f64, Match)>) -> bool {
        let matches = match &self.search.select {
            None => matches,
            Some(sel) => {
                let mut sel = sel.lock().unwrap_or_else(|e| e.into_inner());
                matches
                    .into_iter()
                    .filter_map(|(score, m)| sel.apply_one(m).map(|m| (score, m)))
                    .collect()
            }
        };
        {
            let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = held.as_mut() {
                for (score, m) in matches {
                    h.matches.push((score, h.seq, m));
                    h.seq += 1;
                }
                let limit = self.search.limit;
                if h.matches.len() >= limit.max(64).saturating_mul(2) {
                    h.trim(limit);
                }
                return true;
            }
        }
        self.send(matches.into_iter().map(|(_, m)| m).collect())
            .await
    }

    /// Closes the rank window: sends what it held, best first. Returns
    /// whether matches were dropped for better ones.
    async fn flush(&self) -> bool {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(mut h) = held else {
            return false;
        };
        h.trim(self.search.limit);
        self.send(h.matches.into_iter().map(|(_, _, m)| m).collect())
            .await;
        h.dropped
    }

    /// Projects unscored matches with `select:`, then sends them.
    async fn send_selected(&self, matches: Vec<Match>) -> bool {
        let matches = match &self.search.select {
            None => matches,
            Some(sel) => sel.lock().unwrap_or_else(|e| e.into_inner()).apply(matches),
        };
        self.send(matches).await
    }

    /// Sends up to the remaining limit; false once the limit is reached or
    /// the receiver is gone.
    async fn send(&self, mut matches: Vec<Match>) -> bool {
        self.sending.fetch_add(1, Ordering::SeqCst);
        let _sending = Sending(&self.sending);
        let take = {
            let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
            let take = matches.len().min(self.search.limit.saturating_sub(*sent));
            *sent += take;
            take
        };
        let full = take < matches.len();
        matches.truncate(take);
        // A flushed page can hold megabytes of chunks: split it so the
        // first event goes out (and is parsed) before the rest is encoded.
        while !matches.is_empty() {
            let rest = matches.split_off(event_len(&matches));
            if self.tx.send(Event::Matches(matches)).await.is_err() {
                return false;
            }
            matches = rest;
        }
        !full && !self.limit_hit()
    }

    fn limit_hit(&self) -> bool {
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()) >= self.search.limit
    }
}

/// Most matches, and about the most chunk text, per `matches` event.
const EVENT_MATCHES: usize = 100;
const EVENT_BYTES: usize = 64 << 10;

/// How many of `matches` (at least one) go in the next event.
fn event_len(matches: &[Match]) -> usize {
    let mut bytes = 0;
    for (i, m) in matches.iter().take(EVENT_MATCHES).enumerate() {
        if let Match::Content { chunk_matches, .. } = m {
            bytes += chunk_matches.iter().map(|c| c.content.len()).sum::<usize>();
        }
        if bytes >= EVENT_BYTES {
            return i + 1;
        }
    }
    matches.len().min(EVENT_MATCHES)
}

/// Searches `repos` and sends match events to `tx`. Returns the final
/// progress (with `done` set); the caller sends it and [`Event::Done`].
pub async fn search_repos(
    clients: Arc<RepoClients>,
    repos: Vec<RepoRevs>,
    search: ContentSearch,
    tx: mpsc::Sender<Event>,
) -> Progress {
    let started = Instant::now();
    let repo_count = repos.len();
    let concurrency = search.concurrency.max(1);
    let window = search
        .ranked()
        .then(|| tokio::time::Instant::now() + RANK_WINDOW);
    let shared = Arc::new(Shared {
        started,
        held: Mutex::new(window.map(|_| Held::default())),
        search,
        clients,
        tx,
        sent: Mutex::new(0),
        pending: AtomicUsize::new(0),
        settled: tokio::sync::Notify::new(),
        sending: AtomicUsize::new(0),
    });
    let mut skipped: BTreeMap<SkippedReason, Vec<String>> = BTreeMap::new();
    // Repositories for unindexed search; indexed calls add the branches the
    // index lacks, so the stream ends once they are all done.
    let (fallback, todo) = mpsc::unbounded_channel::<RepoRevs>();
    let use_index = shared.search.commits.is_none() && shared.search.index != YesNoOnly::No;
    let (batches, mut unrouted) = if use_index {
        indexed_batches(&shared.clients, repos)
    } else {
        shared.hold(repos.len());
        for r in repos {
            let _ = fallback.send(r);
        }
        (Vec::new(), Vec::new())
    };
    shared.hold(batches.len());
    skipped
        .entry(SkippedReason::Error)
        .or_default()
        .append(&mut unrouted);
    // Each batch holds a sender clone; the unindexed stream ends after the
    // last batch drops its clone.
    let batches: Vec<_> = batches.into_iter().map(|b| (b, fallback.clone())).collect();
    drop(fallback);
    let indexed = {
        let shared = shared.clone();
        futures::stream::iter(batches)
            .map(move |((node, batch), fallback)| {
                let shared = shared.clone();
                async move {
                    let mut call = Call {
                        shared: &shared,
                        open: true,
                    };
                    search_indexed(&mut call, &node, batch, &fallback).await
                }
            })
            .buffer_unordered(concurrency)
    };
    let unindexed = {
        let shared = shared.clone();
        UnboundedReceiverStream::new(todo)
            .map(move |repo| {
                let shared = shared.clone();
                async move {
                    let name = repo.name.clone();
                    let result = search_repo(&shared, repo).await;
                    shared.settle();
                    match result {
                        Ok(()) => Vec::new(),
                        Err(reason) => vec![(name, reason)],
                    }
                }
            })
            .buffer_unordered(concurrency)
    };
    let mut results = futures::stream::select(indexed.boxed(), unindexed.boxed());
    let mut window = window;
    let mut dropped = false;
    loop {
        let next = match window {
            Some(deadline) => tokio::select! {
                next = results.next() => next,
                () = tokio::time::sleep_until(deadline) => {
                    window = None;
                    dropped |= shared.flush().await;
                    continue;
                }
                () = shared.settled.notified() => {
                    window = None;
                    dropped |= shared.flush().await;
                    continue;
                }
            },
            None => results.next().await,
        };
        let Some(failed) = next else {
            break;
        };
        for (name, reason) in failed {
            skipped.entry(reason).or_default().push(name);
        }
        // Stop at the limit once the matches counted are all sent: a send
        // can be waiting on `tx` with the rest of its events.
        if shared.limit_hit() && shared.sending.load(Ordering::SeqCst) == 0 {
            break;
        }
    }
    dropped |= shared.flush().await;
    if dropped || shared.limit_hit() {
        skipped.entry(SkippedReason::DisplayLimit).or_default();
    }
    skipped.retain(|reason, names| *reason == SkippedReason::DisplayLimit || !names.is_empty());
    let match_count = *shared.sent.lock().unwrap_or_else(|e| e.into_inner()) as u64;
    Progress {
        done: true,
        repositories_count: Some(u32::try_from(repo_count).unwrap_or(u32::MAX)),
        match_count,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        skipped: skipped
            .into_iter()
            .map(|(reason, names)| skipped_entry(reason, &names))
            .collect(),
    }
}

pub(crate) fn skipped_entry(reason: SkippedReason, names: &[String]) -> Skipped {
    let n = names.len();
    let (title, severity) = match reason {
        SkippedReason::DisplayLimit => ("display limit hit".to_owned(), Severity::Info),
        SkippedReason::RepositoryMissing => (format!("{n} missing"), Severity::Info),
        SkippedReason::RepositoryCloning => (format!("{n} cloning"), Severity::Info),
        SkippedReason::ShardTimeout => (format!("{n} timed out"), Severity::Warn),
        _ => (format!("{n} errored"), Severity::Warn),
    };
    let message = match reason {
        SkippedReason::DisplayLimit => {
            "Not all results were shown. Use `count:` to see more.".to_owned()
        }
        _ => names
            .iter()
            .map(|n| format!("- `{n}`"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    Skipped {
        reason,
        title,
        message,
        severity,
        suggested: None,
    }
}

pub(crate) fn skip_reason(status: &Status) -> SkippedReason {
    match ErrorKind::of(status) {
        Some(ErrorKind::RepoNotFound) => SkippedReason::RepositoryMissing,
        Some(ErrorKind::CloneInProgress) => SkippedReason::RepositoryCloning,
        Some(ErrorKind::Timeout) => SkippedReason::ShardTimeout,
        _ if status.code() == sourcegull_rpc::tonic::Code::DeadlineExceeded => {
            SkippedReason::ShardTimeout
        }
        // The client channel ends a call whose gRPC timeout passed with `Cancelled`.
        _ if status.code() == sourcegull_rpc::tonic::Code::Cancelled
            && status.message() == sourcegull_rpc::tonic::TimeoutExpired(()).to_string() =>
        {
            SkippedReason::ShardTimeout
        }
        _ => SkippedReason::Error,
    }
}

/// A call carrying what is left until `deadline` as its gRPC timeout.
pub(crate) fn with_deadline<T>(
    msg: T,
    deadline: Option<Instant>,
) -> sourcegull_rpc::tonic::Request<T> {
    let mut req = sourcegull_rpc::tonic::Request::new(msg);
    if let Some(d) = deadline {
        req.set_timeout(d.saturating_duration_since(Instant::now()));
    }
    req
}

/// The flush wall time, in ms, of a ranked call starting `elapsed` after the
/// search: what is left of [`FIRST_PAGE`], at least [`MIN_FLUSH`].
fn flush_ms(elapsed: Duration) -> u32 {
    let flush = FIRST_PAGE.saturating_sub(elapsed).max(MIN_FLUSH);
    u32::try_from(flush.as_millis()).unwrap_or(u32::MAX)
}

/// Groups repositories by owning node in batches of [`INDEXED_BATCH`];
/// also returns the names of repositories no node owns.
pub(crate) fn indexed_batches(
    clients: &RepoClients,
    repos: Vec<RepoRevs>,
) -> (Vec<(String, Vec<RepoRevs>)>, Vec<String>) {
    let router = clients.router();
    let mut by_node: HashMap<&str, Vec<RepoRevs>> = HashMap::new();
    let mut order = Vec::new();
    let mut unrouted = Vec::new();
    for r in repos {
        match router.node_for(RepoId(r.id)) {
            Some(node) => {
                let group = by_node.entry(node).or_insert_with(|| {
                    order.push(node);
                    Vec::new()
                });
                group.push(r);
            }
            None => unrouted.push(r.name),
        }
    }
    let mut batches = Vec::new();
    for node in order {
        let mut group = by_node.remove(node).unwrap_or_default();
        while !group.is_empty() {
            let rest = group.split_off(group.len().min(INDEXED_BATCH));
            batches.push((node.to_owned(), std::mem::replace(&mut group, rest)));
        }
    }
    (batches, unrouted)
}

/// The index branch for a requested revision: the default branch is `HEAD`.
pub(crate) fn index_branch(rev: &str) -> &str {
    if rev.is_empty() { DEFAULT_BRANCH } else { rev }
}

/// An indexed call, counted in [`Shared::pending`] until it sends its
/// first files or ends.
struct Call<'a> {
    shared: &'a Shared,
    open: bool,
}

impl Call<'_> {
    fn settle(&mut self) {
        if std::mem::take(&mut self.open) {
            self.shared.settle();
        }
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        self.settle();
    }
}

/// Searches `repos` on `node`'s index, sending the branches it lacks to
/// `fallback` (unless `index:only`). Returns the repositories that failed.
async fn search_indexed(
    call: &mut Call<'_>,
    node: &str,
    repos: Vec<RepoRevs>,
    fallback: &mpsc::UnboundedSender<RepoRevs>,
) -> Vec<(String, SkippedReason)> {
    let shared = call.shared;
    let s = &shared.search;
    let to_fallback = |repo: RepoRevs| {
        if s.index != YesNoOnly::Only {
            shared.hold(1);
            if fallback.send(repo).is_err() {
                shared.settle();
            }
        }
    };
    let fail_all = |repos: &HashMap<i32, RepoRevs>, reason: SkippedReason| -> Vec<_> {
        repos.values().map(|r| (r.name.clone(), reason)).collect()
    };
    let req = pb::SearchIndexedRequest {
        repos: repos
            .iter()
            .map(|r| pb::IndexedRepoBranches {
                repo: Some(pb::RepoRef {
                    id: r.id,
                    name: r.name.clone(),
                }),
                branches: if r.revs.is_empty() {
                    vec![DEFAULT_BRANCH.to_owned()]
                } else {
                    r.revs.iter().map(|v| index_branch(v).to_owned()).collect()
                },
            })
            .collect(),
        pattern: s.pattern.as_ref().map(spec_to_pb),
        case_sensitive: s.case_sensitive,
        paths: Some(path_filter_to_pb(&s.paths)),
        max_line_len: s.max_line_len,
        file_limit: u32::try_from(s.limit).unwrap_or(u32::MAX),
        path_pattern: s.path_pattern.as_ref().map(spec_to_pb),
        symbols: s.symbols,
        bm25: s.bm25,
        owners: s
            .owners
            .as_ref()
            .map(|o| o.to_pb(repos.iter().map(|r| r.id))),
        repos_only: false,
        flush_ms: if s.ranked() {
            flush_ms(shared.started.elapsed())
        } else {
            0
        },
    };
    let by_id: HashMap<i32, RepoRevs> = repos.into_iter().map(|r| (r.id, r)).collect();
    let mut client = match shared.clients.search_for_node(node) {
        Ok(c) => c,
        Err(_) => return fail_all(&by_id, SkippedReason::Error),
    };
    let mut stream = match client.search_indexed(with_deadline(req, s.deadline)).await {
        Ok(r) => r.into_inner(),
        Err(status) if status.code() == sourcegull_rpc::tonic::Code::Unimplemented => {
            by_id.into_values().for_each(to_fallback);
            return Vec::new();
        }
        Err(status) => return fail_all(&by_id, skip_reason(&status)),
    };
    let path_search = s.pattern.is_none() && s.path_pattern.is_some();
    loop {
        let msg = match stream.message().await {
            Ok(Some(m)) => m,
            Ok(None) => return Vec::new(),
            Err(status) => return fail_all(&by_id, skip_reason(&status)),
        };
        for m in msg.missing {
            let Some(repo) = m.repo.and_then(|r| by_id.get(&r.id)) else {
                continue;
            };
            // Back to the revisions as requested ("" for the default branch).
            let revs = m
                .branches
                .iter()
                .filter_map(|b| repo.revs.iter().find(|v| index_branch(v) == b).cloned())
                .collect();
            to_fallback(RepoRevs {
                id: repo.id,
                name: repo.name.clone(),
                revs,
                stars: repo.stars,
            });
        }
        let msg_files_empty = msg.files.is_empty();
        let mut matches = Vec::with_capacity(msg.files.len());
        for f in msg.files {
            let (Some(repo), Some(file)) = (by_id.get(&f.repo_id), f.file) else {
                continue;
            };
            let score = file.score + ranking::repo_score(repo.stars);
            if reports_owners(s) {
                matches.extend(owner_matches(&file.owners).into_iter().map(|m| (score, m)));
                continue;
            }
            // One match per requested revision, as unindexed search returns.
            for b in &f.branches {
                let rev = repo
                    .revs
                    .iter()
                    .find(|v| index_branch(v) == b.name)
                    .map_or("", String::as_str);
                let m = to_match(repo, &b.commit_oid, rev, path_search, file.clone());
                matches.push((score, m));
            }
        }
        let had_files = !msg_files_empty;
        if !shared.send_scored(matches).await {
            return Vec::new();
        }
        if msg.stats.is_some_and(|st| st.canceled) {
            return fail_all(&by_id, SkippedReason::ShardTimeout);
        }
        // The node sends its best matches first: later ones need not
        // be ranked against them.
        if had_files {
            call.settle();
        }
    }
}

async fn search_repo(shared: &Shared, repo: RepoRevs) -> Result<(), SkippedReason> {
    // `^rev` leaves out a revision's history (`repo:r@new:^old`), for commit and diff search
    // only: file search has nothing to leave out.
    let (excluded, mut revs): (Vec<String>, Vec<String>) =
        repo.revs.iter().cloned().partition(|r| r.starts_with('^'));
    if revs.is_empty() {
        revs.push(String::new());
    }
    let id = RepoId(repo.id);
    let repo_ref = pb::RepoRef {
        id: repo.id,
        name: repo.name.clone(),
    };
    let mut hidden = Vec::new();
    if shared.search.commits.is_some() {
        for rev in excluded {
            let rev = &rev[1..];
            if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
                // A commit id needs no round trip; the node ignores unknown ones.
                hidden.push(rev.to_ascii_lowercase());
                continue;
            }
            let mut git = shared
                .clients
                .for_repo(id)
                .map_err(|_| SkippedReason::Error)?;
            let oid = git
                .resolve_revision(pb::ResolveRevisionRequest {
                    repo: Some(repo_ref.clone()),
                    rev: rev.to_owned(),
                })
                .await
                .map_err(|s| skip_reason(&s))?
                .into_inner()
                .commit_oid;
            hidden.push(oid);
        }
    }
    for rev in revs {
        if shared.limit_hit() {
            return Ok(());
        }
        let mut git = shared
            .clients
            .for_repo(id)
            .map_err(|_| SkippedReason::Error)?;
        let commit = match git
            .resolve_revision(pb::ResolveRevisionRequest {
                repo: Some(repo_ref.clone()),
                rev: rev.clone(),
            })
            .await
        {
            Ok(r) => r.into_inner().commit_oid,
            // An empty repository (no commits yet, or no repository on the code host yet)
            // has no default branch: nothing to search, and not an error.
            Err(s) if rev.is_empty() && ErrorKind::of(&s) == Some(ErrorKind::RevisionNotFound) => {
                continue;
            }
            Err(s) => return Err(skip_reason(&s)),
        };
        let mut search = shared
            .clients
            .search_for_repo(id)
            .map_err(|_| SkippedReason::Error)?;
        let s = &shared.search;
        if let Some(filters) = &s.commits {
            let req = pb::SearchCommitsRequest {
                repo: Some(repo_ref.clone()),
                commit_oid: commit.clone(),
                pattern: s.pattern.as_ref().map(spec_to_pb),
                case_sensitive: s.case_sensitive,
                filters: Some(commit_filters_to_pb(filters)),
                limit: u32::try_from(s.limit).unwrap_or(u32::MAX),
                diff: s.diff,
                paths: (!s.paths.is_empty()).then(|| path_filter_to_pb(&s.paths)),
                hide_oids: hidden.clone(),
            };
            let mut stream = search
                .search_commits(with_deadline(req, s.deadline))
                .await
                .map_err(|s| skip_reason(&s))?
                .into_inner();
            let mut canceled = false;
            while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
                canceled |= msg.canceled;
                let matches = msg
                    .commits
                    .into_iter()
                    .filter_map(|c| match s.diff_lines {
                        Some(sign) => diff_lines(c, sign),
                        None => Some(c),
                    })
                    .map(|c| to_commit_match(&repo, c, s.diff))
                    .collect();
                if !shared.send_selected(matches).await {
                    return Ok(());
                }
            }
            if canceled {
                return Err(SkippedReason::ShardTimeout);
            }
            continue;
        }
        let path_search = s.pattern.is_none() && s.path_pattern.is_some();
        let mut canceled = false;
        let req = pb::SearchCommitRequest {
            repo: Some(repo_ref.clone()),
            commit_oid: commit.clone(),
            pattern: s.pattern.as_ref().map(spec_to_pb),
            case_sensitive: s.case_sensitive,
            paths: Some(path_filter_to_pb(&s.paths)),
            max_file_size: 0,
            max_line_len: s.max_line_len,
            file_limit: u32::try_from(s.limit).unwrap_or(u32::MAX),
            path_pattern: s.path_pattern.as_ref().map(spec_to_pb),
            symbols: s.symbols,
            bm25: s.bm25,
            owners: s.owners.as_ref().map(|o| o.to_pb([repo.id])),
        };
        let mut stream = search
            .search_commit(with_deadline(req, s.deadline))
            .await
            .map_err(|s| skip_reason(&s))?
            .into_inner();
        while let Some(msg) = stream.message().await.map_err(|s| skip_reason(&s))? {
            if msg.stats.is_some_and(|st| st.canceled) {
                canceled = true;
            }
            let repo_score = ranking::repo_score(repo.stars);
            let matches = msg
                .files
                .into_iter()
                .flat_map(|f| {
                    let score = f.score + repo_score;
                    if reports_owners(s) {
                        return owner_matches(&f.owners)
                            .into_iter()
                            .map(|m| (score, m))
                            .collect::<Vec<_>>();
                    }
                    vec![(score, to_match(&repo, &commit, &rev, path_search, f))]
                })
                .collect();
            if !shared.send_scored(matches).await {
                return Ok(());
            }
        }
        if canceled {
            return Err(SkippedReason::ShardTimeout);
        }
    }
    Ok(())
}

fn reports_owners(s: &ContentSearch) -> bool {
    s.owners.as_ref().is_some_and(|o| o.report)
}

/// The owner results of a file's owners as the node reports them (`@handle` or an email),
/// not yet resolved: `web` resolves them to users and teams (`select:file.owners`).
pub fn owner_matches(owners: &[String]) -> Vec<Match> {
    owners
        .iter()
        .map(|o| {
            let (handle, email) = match o.strip_prefix('@') {
                Some(h) => (h.to_owned(), String::new()),
                None if o.contains('@') => (String::new(), o.clone()),
                None => (o.clone(), String::new()),
            };
            Match::Person {
                handle,
                email,
                user: None,
            }
        })
        .collect()
}

/// Keeps only a diff match's lines starting with `sign` (`+` or `-`) that
/// hold a match range (`select:commit.diff.added`/`removed`); `None` when
/// none are left.
pub fn diff_lines(mut m: pb::SearchCommitMatch, sign: char) -> Option<pb::SearchCommitMatch> {
    for f in &mut m.diff {
        for h in &mut f.hunks {
            let lines: Vec<&str> = h.body.split_inclusive('\n').collect();
            let mut keep = vec![false; lines.len()];
            for r in &h.ranges {
                let line = r.start.unwrap_or_default().line as usize;
                if lines.get(line).is_some_and(|l| l.starts_with(sign)) {
                    keep[line] = true;
                }
            }
            // New line numbers for the kept lines.
            let mut renumber = vec![0u32; lines.len()];
            let mut body = String::new();
            let mut n = 0;
            for (i, l) in lines.iter().enumerate() {
                if keep[i] {
                    renumber[i] = n;
                    n += 1;
                    body.push_str(l);
                }
            }
            h.ranges
                .retain(|r| keep[r.start.unwrap_or_default().line as usize]);
            for r in &mut h.ranges {
                for loc in [&mut r.start, &mut r.end].into_iter().flatten() {
                    loc.line = renumber.get(loc.line as usize).copied().unwrap_or(loc.line);
                }
            }
            h.body = body;
        }
        f.hunks.retain(|h| !h.ranges.is_empty());
    }
    m.diff.retain(|f| !f.hunks.is_empty());
    (!m.diff.is_empty()).then_some(m)
}

fn rfc3339(s: Option<&pb::Signature>) -> String {
    s.and_then(|s| chrono::DateTime::from_timestamp(s.time, 0))
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Converts a node's commit match to a stream match. A diff match's content
/// is its files and hunks in a `diff` code fence: an
/// `old new` line per file (`/dev/null` for a missing side, spaces escaped
/// as `\ `), then per hunk `@@ -a,b +c,d @@ <section>` (the space is there
/// even when the section is empty) and its lines.
pub fn to_commit_match(repo: &RepoRevs, m: pb::SearchCommitMatch, diff: bool) -> Match {
    let c = m.commit.unwrap_or_default();
    let author = c
        .author
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let committer = c
        .committer
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let author_date = rfc3339(c.author.as_ref());
    let url = format!("/{}/-/commit/{}", repo.name, c.oid);
    let subject = c.message.lines().next().unwrap_or_default().to_owned();
    let short = c.oid.get(..7).unwrap_or(&c.oid).to_owned();
    // `[line, column, length]` with `line` counted in `content`.
    let triple = |r: pb::SearchRange, first_line: u32| {
        let (s, e) = (r.start.unwrap_or_default(), r.end.unwrap_or_default());
        [
            first_line + s.line,
            s.column,
            e.offset.saturating_sub(s.offset),
        ]
    };
    let (content, ranges) = if diff {
        let mut text = String::new();
        let mut ranges = Vec::new();
        // One line down for the opening fence.
        let mut line = 1u32;
        for f in m.diff {
            let side = |p: String| {
                if p.is_empty() {
                    "/dev/null".to_owned()
                } else {
                    p
                }
            };
            text.push_str(&format!(
                "{} {}\n",
                side(f.old_path).replace(' ', "\\ "),
                side(f.new_path).replace(' ', "\\ ")
            ));
            line += 1;
            for h in f.hunks {
                text.push_str(&format!(
                    "@@ -{},{} +{},{} @@ {}\n",
                    h.old_start, h.old_lines, h.new_start, h.new_lines, h.section
                ));
                line += 1;
                ranges.extend(h.ranges.into_iter().map(|r| triple(r, line)));
                text.push_str(&h.body);
                line += u32::try_from(h.body.lines().count()).unwrap_or(u32::MAX);
            }
        }
        (
            format!("```diff\n{}\n```", text.trim_end_matches('\n')),
            ranges,
        )
    } else {
        let ranges = m.message_ranges.into_iter().map(|r| triple(r, 1)).collect();
        (
            format!("```COMMIT_EDITMSG\n{}\n```", c.message.trim_end()),
            ranges,
        )
    };
    Match::Commit {
        label: format!(
            "[{}](/{}) › [{author}]({url}): [{subject}]({url})",
            repo.name, repo.name
        ),
        detail: format!("[`{short}` {}]({url})", author_date.get(..10).unwrap_or("")),
        url,
        repository_id: repo.id,
        repository: repo.name.clone(),
        content,
        oid: c.oid,
        message: c.message,
        author_name: author,
        author_date,
        committer_name: committer,
        committer_date: rfc3339(c.committer.as_ref()),
        ranges,
    }
}

fn loc(l: Option<pb::SearchLocation>) -> stream::Location {
    let l = l.unwrap_or_default();
    stream::Location {
        offset: l.offset,
        line: l.line,
        column: l.column,
    }
}

fn range(r: pb::SearchRange) -> stream::Range {
    stream::Range {
        start: loc(r.start),
        end: loc(r.end),
    }
}

/// Converts a node's file match to a stream match: a path match when the
/// search had a path pattern and no content pattern, else a content match.
pub fn to_match(
    repo: &RepoRevs,
    commit: &str,
    rev: &str,
    path_search: bool,
    f: pb::SearchFileMatch,
) -> Match {
    let language = sourcegull_languages::detect(&f.path, None).map(|l| l.name.to_owned());
    let branches = if rev.is_empty() {
        Vec::new()
    } else {
        vec![rev.to_owned()]
    };
    if !f.symbols.is_empty() {
        // `/repo[@rev]/-/blob/path#L<line>:<col>-<line>:<col>`, 1-based.
        let file_url = if rev.is_empty() {
            format!("/{}/-/blob/{}", repo.name, f.path)
        } else {
            format!("/{}@{rev}/-/blob/{}", repo.name, f.path)
        };
        let symbols = f
            .symbols
            .into_iter()
            .map(|s| {
                let r = range(s.range.unwrap_or_default());
                stream::Symbol {
                    url: format!(
                        "{file_url}#L{}:{}-{}:{}",
                        r.start.line + 1,
                        r.start.column + 1,
                        r.end.line + 1,
                        r.end.column + 1
                    ),
                    name: s.name,
                    container_name: s.container,
                    kind: SymbolKind::from_u8(u8::try_from(s.kind).unwrap_or(0))
                        .name()
                        .to_owned(),
                    line: r.start.line + 1,
                    range: r,
                }
            })
            .collect();
        return Match::Symbol {
            path: f.path,
            repository_id: repo.id,
            repository: repo.name.clone(),
            commit: commit.to_owned(),
            branches,
            language,
            symbols,
        };
    }
    if path_search {
        return Match::Path {
            path: f.path,
            path_matches: f.path_ranges.into_iter().map(range).collect(),
            repository_id: repo.id,
            repository: repo.name.clone(),
            commit: commit.to_owned(),
            branches,
            language,
        };
    }
    Match::Content {
        repository_id: repo.id,
        repository: repo.name.clone(),
        commit: commit.to_owned(),
        branches,
        chunk_matches: f
            .chunks
            .into_iter()
            .map(|c| ChunkMatch {
                content: c.content,
                content_start: loc(c.content_start),
                ranges: c.ranges.into_iter().map(range).collect(),
            })
            .collect(),
        path: f.path,
        language,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_call_timeouts_are_shard_timeouts() {
        use sourcegull_rpc::tonic::TimeoutExpired;
        let expired = Status::cancelled(TimeoutExpired(()).to_string());
        assert_eq!(skip_reason(&expired), SkippedReason::ShardTimeout);
        let deadline = Status::deadline_exceeded("deadline");
        assert_eq!(skip_reason(&deadline), SkippedReason::ShardTimeout);
        assert_eq!(
            skip_reason(&Status::cancelled("client went away")),
            SkippedReason::Error
        );
    }

    #[test]
    fn calls_flush_by_the_first_page() {
        let ms = |d: u64| flush_ms(Duration::from_millis(d));
        assert_eq!(ms(0), 75);
        assert_eq!(ms(30), 45);
        // A call that starts late streams almost at once.
        assert_eq!(ms(70), 10);
        assert_eq!(ms(500), 10);
        assert!(FIRST_PAGE < RANK_WINDOW);
    }

    #[test]
    fn large_pages_are_split_into_events() {
        let file = |bytes: usize| Match::Content {
            path: "a".into(),
            repository_id: 1,
            repository: "r".into(),
            commit: "c".into(),
            branches: Vec::new(),
            chunk_matches: vec![ChunkMatch {
                content: "x".repeat(bytes),
                content_start: stream::Location {
                    offset: 0,
                    line: 0,
                    column: 0,
                },
                ranges: Vec::new(),
            }],
            language: None,
        };
        let small: Vec<Match> = (0..250).map(|_| file(10)).collect();
        assert_eq!(event_len(&small), EVENT_MATCHES);
        assert_eq!(event_len(&small[..7]), 7);
        let big: Vec<Match> = (0..10).map(|_| file(EVENT_BYTES / 2)).collect();
        assert_eq!(event_len(&big), 2);
        assert_eq!(event_len(&[file(EVENT_BYTES * 4)]), 1);
    }

    #[test]
    fn converts_file_matches() {
        let repo = RepoRevs {
            id: 4,
            name: "gitlab.com/a/b".into(),
            revs: Vec::new(),
            stars: 0,
        };
        let m = to_match(
            &repo,
            "abc",
            "dev",
            false,
            pb::SearchFileMatch {
                path: "x.go".into(),
                blob_oid: "def".into(),
                chunks: vec![pb::SearchChunk {
                    content: "foo".into(),
                    content_start: None,
                    ranges: Vec::new(),
                }],
                path_ranges: Vec::new(),
                symbols: Vec::new(),
                score: 0.0,
                owners: Vec::new(),
            },
        );
        let Match::Content {
            branches,
            language,
            chunk_matches,
            ..
        } = m
        else {
            panic!("content match");
        };
        assert_eq!(branches, ["dev"]);
        assert_eq!(language.as_deref(), Some("Go"));
        assert_eq!(chunk_matches[0].content, "foo");
    }

    #[test]
    fn converts_symbol_matches() {
        let repo = RepoRevs {
            id: 4,
            name: "gitlab.com/a/b".into(),
            revs: Vec::new(),
            stars: 0,
        };
        let at = |line, column| {
            Some(pb::SearchLocation {
                offset: 0,
                line,
                column,
            })
        };
        let file = |rev| {
            to_match(
                &repo,
                "abc",
                rev,
                false,
                pb::SearchFileMatch {
                    path: "srv/server.go".into(),
                    symbols: vec![pb::SearchSymbol {
                        name: "Run".into(),
                        container: "Server".into(),
                        kind: 6,
                        range: Some(pb::SearchRange {
                            start: at(9, 17),
                            end: at(9, 20),
                        }),
                    }],
                    ..Default::default()
                },
            )
        };
        let json = serde_json::to_value(file("")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "symbol",
                "path": "srv/server.go",
                "repositoryID": 4,
                "repository": "gitlab.com/a/b",
                "commit": "abc",
                "language": "Go",
                "symbols": [{
                    "url": "/gitlab.com/a/b/-/blob/srv/server.go#L10:18-10:21",
                    "name": "Run",
                    "containerName": "Server",
                    "kind": "METHOD",
                    "line": 10,
                }],
            })
        );
        let Match::Symbol { symbols, .. } = file("dev") else {
            panic!("symbol match");
        };
        assert!(symbols[0].url.starts_with("/gitlab.com/a/b@dev/-/blob/"));
    }

    #[test]
    fn renders_diff_matches() {
        let repo = RepoRevs {
            id: 1,
            name: "r".into(),
            revs: Vec::new(),
            stars: 0,
        };
        let at = |offset, line, column| {
            Some(pb::SearchLocation {
                offset,
                line,
                column,
            })
        };
        let m = pb::SearchCommitMatch {
            commit: Some(pb::Commit {
                oid: "abcdef12".into(),
                message: "fix\n".into(),
                ..Default::default()
            }),
            message_ranges: Vec::new(),
            diff: vec![pb::SearchDiffFile {
                old_path: String::new(),
                new_path: "a b.go".into(),
                hunks: vec![pb::SearchDiffHunk {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 2,
                    body: "+package a\n+// hello\n".into(),
                    section: String::new(),
                    // "hello" on the hunk's second line.
                    ranges: vec![pb::SearchRange {
                        start: at(15, 1, 4),
                        end: at(20, 1, 9),
                    }],
                }],
            }],
        };
        let Match::Commit {
            content, ranges, ..
        } = to_commit_match(&repo, m, true)
        else {
            panic!("commit match");
        };
        assert_eq!(
            content,
            "```diff\n/dev/null a\\ b.go\n@@ -0,0 +1,2 @@ \n+package a\n+// hello\n```"
        );
        assert_eq!(ranges, [[4, 4, 5]]);
        let line = content.lines().nth(4).unwrap();
        assert_eq!(&line[4..9], "hello");
    }

    #[test]
    fn held_matches_keep_the_best_in_order() {
        let m = |id| Match::Repo {
            repository_id: id,
            repository: String::new(),
            description: None,
            fork: false,
            archived: false,
            private: false,
        };
        let mut h = Held::default();
        for (seq, (score, id)) in [(1.0, 1), (5.0, 2), (3.0, 3), (5.0, 4)]
            .into_iter()
            .enumerate()
        {
            h.matches.push((score, seq, m(id)));
        }
        h.trim(3);
        let ids: Vec<_> = h
            .matches
            .iter()
            .map(|(_, _, m)| match m {
                Match::Repo { repository_id, .. } => *repository_id,
                _ => unreachable!(),
            })
            .collect();
        // Best first; equal scores in arrival order.
        assert_eq!(ids, [2, 4, 3]);
        assert!(h.dropped);
    }

    #[test]
    fn keeps_added_lines_that_matched() {
        let at = |line| {
            Some(pb::SearchLocation {
                offset: 0,
                line,
                column: 1,
            })
        };
        let range = |line| pb::SearchRange {
            start: at(line),
            end: at(line),
        };
        let m = pb::SearchCommitMatch {
            commit: Some(pb::Commit::default()),
            message_ranges: Vec::new(),
            diff: vec![pb::SearchDiffFile {
                old_path: "a".into(),
                new_path: "a".into(),
                hunks: vec![
                    pb::SearchDiffHunk {
                        body: " ctx foo\n-old foo\n+new foo\n+other\n".into(),
                        ranges: vec![range(0), range(1), range(2)],
                        ..Default::default()
                    },
                    pb::SearchDiffHunk {
                        body: "-gone foo\n".into(),
                        ranges: vec![range(0)],
                        ..Default::default()
                    },
                ],
            }],
        };
        let added = diff_lines(m.clone(), '+').unwrap();
        let hunks = &added.diff[0].hunks;
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].body, "+new foo\n");
        assert_eq!(hunks[0].ranges[0].start.unwrap().line, 0);
        let removed = diff_lines(m, '-').unwrap();
        assert_eq!(removed.diff[0].hunks.len(), 2);
        assert_eq!(removed.diff[0].hunks[0].body, "-old foo\n");
        let none = pb::SearchCommitMatch {
            diff: Vec::new(),
            ..Default::default()
        };
        assert!(diff_lines(none, '+').is_none());
    }

    #[test]
    fn display_limit_message() {
        let s = skipped_entry(SkippedReason::RepositoryMissing, &["a".into(), "b".into()]);
        assert_eq!(s.title, "2 missing");
        assert_eq!(s.message, "- `a`\n- `b`");
    }
}
