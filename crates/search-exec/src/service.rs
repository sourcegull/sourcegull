//! The repo node's `SearchService`, served by the repo
//! role next to `RepoService`.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;

use sourcegull_gitstore::{GitError, GitStore, read};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_rpc::repo::v1::search_service_server::{SearchService, SearchServiceServer};
use sourcegull_search::Matcher;
use sourcegull_search::index::{
    Cancel, CancelOnDrop, IndexQuery, IndexSearchStats, IndexedFile, RepoBranches, SearchIndex,
};
use sourcegull_search_client::{
    OwnerSearch, commit_filters_from_pb, path_filter_from_pb, spec_from_pb,
};
use sourcegull_types::{AlwaysIndex, RepoId};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::owners::{OwnerCheck, OwnerSearchExt};
use crate::symbol_cache::SymbolCache;
use crate::unindexed::search_commit_symbols_cached;
use crate::{
    ChunkMatch, CommitFilters, CommitMatch, CommitSearchOptions, CompileError,
    DEFAULT_MAX_FILE_SIZE, DiffCache, DiffFileMatch, FileMatch, Location, PathFilter, SearchStats,
    SymbolMatch, TrigramQuery, UnindexedOptions, chunk_matches, search_commit, search_commits,
    trigram_query,
};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// The response stream over `rx`, canceling its search when dropped: when the
/// client cancels the call or goes away, the server drops the stream.
fn cancel_on_drop<T: Send + 'static>(
    rx: mpsc::Receiver<Result<T, Status>>,
    guard: CancelOnDrop,
) -> BoxStream<T> {
    Box::pin(ReceiverStream::new(rx).map(move |item| {
        let _ = &guard;
        item
    }))
}

/// The cancel flag of a search call: set when its response stream is dropped
/// and at the deadline the client sent.
fn call_cancel<T>(req: &Request<T>) -> Cancel {
    Cancel::new().with_deadline(sourcegull_rpc::request_deadline(req))
}

/// Files per `SearchCommit` message.
const FILE_BATCH: usize = 50;

/// Commits per `SearchCommits` message.
const COMMIT_BATCH: usize = 50;

/// Memory for rendered diffs of recent commits (diff search).
const DIFF_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const SYMBOL_CACHE_BYTES: u64 = 128 * 1024 * 1024;
/// Memory for per-file counts of insight series.
const SERIES_CACHE_BYTES: u64 = 64 * 1024 * 1024;
/// Blobs whose line counts are kept for language statistics.
const LOC_CACHE_ENTRIES: u64 = 1 << 20;

/// The node's indexer as the site admin RPCs see it (`IndexStatus`,
/// `Reindex`): implemented by the repo role over its index queue.
pub trait IndexControl: Send + Sync + 'static {
    /// Whether a build of `repo` is queued.
    fn queued(&self, repo: RepoId) -> bool;
    /// Queues a full rebuild of `repo`'s index.
    fn reindex(&self, repo: RepoId);
}

pub struct SearchNodeService {
    store: Arc<GitStore>,
    index: Option<Arc<dyn SearchIndex>>,
    control: Option<Arc<dyn IndexControl>>,
    diffs: DiffCache,
    symbols: SymbolCache,
    series: crate::series::CountCache,
    loc: crate::langstats::LocCache,
    always_index: Option<AlwaysIndexFn>,
}

/// The `search.index.alwaysIndex` rule of a repository, by name.
pub type AlwaysIndexFn = Arc<dyn Fn(&str) -> AlwaysIndex + Send + Sync>;

impl SearchNodeService {
    pub fn new(store: Arc<GitStore>) -> Self {
        Self {
            store,
            index: None,
            control: None,
            diffs: DiffCache::new(DIFF_CACHE_BYTES),
            symbols: SymbolCache::new(SYMBOL_CACHE_BYTES),
            series: crate::series::CountCache::new(SERIES_CACHE_BYTES),
            loc: crate::langstats::LocCache::new(LOC_CACHE_ENTRIES),
            always_index: None,
        }
    }

    /// Serves `SearchIndexed` from `index`.
    #[must_use]
    pub fn with_index(mut self, index: Arc<dyn SearchIndex>) -> Self {
        self.index = Some(index);
        self
    }

    /// Answers `Reindex` and the `queued` flag of `IndexStatus` through `control`.
    #[must_use]
    pub fn with_control(mut self, control: Arc<dyn IndexControl>) -> Self {
        self.control = Some(control);
        self
    }

    /// Searches files matched by `rule` above the size limit in unindexed
    /// search, as the index does.
    #[must_use]
    pub fn with_always_index(mut self, rule: AlwaysIndexFn) -> Self {
        self.always_index = Some(rule);
        self
    }

    pub fn into_server(self) -> SearchServiceServer<Self> {
        SearchServiceServer::new(self).max_encoding_message_size(64 * 1024 * 1024)
    }
}

/// The wire form of a repository's index status.
fn index_status_to_pb(
    status: Option<sourcegull_search::index::RepoIndexStatus>,
    queued: bool,
) -> pb::IndexStatusResponse {
    let Some(s) = status else {
        return pb::IndexStatusResponse {
            queued,
            ..Default::default()
        };
    };
    let updated_at = s
        .updated_at
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    pb::IndexStatusResponse {
        indexed: true,
        branches: s
            .branches
            .into_iter()
            .map(|b| pb::IndexedBranch {
                name: b.name,
                commit_oid: b.commit,
            })
            .collect(),
        shards: s
            .shards
            .into_iter()
            .map(|f| pb::IndexShard {
                kind: f.kind,
                file: f.file,
                size_bytes: f.size_bytes,
                docs: f.docs,
                repos: f.repos,
            })
            .collect(),
        docs: s.docs,
        content_bytes: s.content_bytes,
        index_bytes: s.index_bytes,
        lines: s.lines,
        default_branch_lines: s.default_branch_lines,
        updated_at,
        queued,
        ranks_generation: s.ranks_generation,
    }
}

fn invalid(e: CompileError) -> Status {
    Status::invalid_argument(e.to_string())
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn loc_to_pb(l: Location) -> pb::SearchLocation {
    pb::SearchLocation {
        offset: to_u32(l.offset),
        line: to_u32(l.line),
        column: to_u32(l.column),
    }
}

fn chunk_to_pb(c: ChunkMatch) -> pb::SearchChunk {
    pb::SearchChunk {
        content: c.content,
        content_start: Some(loc_to_pb(c.content_start)),
        ranges: c
            .ranges
            .into_iter()
            .map(|r| pb::SearchRange {
                start: Some(loc_to_pb(r.start)),
                end: Some(loc_to_pb(r.end)),
            })
            .collect(),
    }
}

fn symbol_to_pb(s: SymbolMatch) -> pb::SearchSymbol {
    pb::SearchSymbol {
        name: s.name,
        container: s.container.unwrap_or_default(),
        kind: u32::from(s.kind),
        range: Some(pb::SearchRange {
            start: Some(loc_to_pb(s.range.start)),
            end: Some(loc_to_pb(s.range.end)),
        }),
    }
}

fn file_to_pb(f: FileMatch) -> pb::SearchFileMatch {
    let at = |offset: usize| {
        Some(pb::SearchLocation {
            offset: to_u32(offset),
            line: 0,
            column: to_u32(offset),
        })
    };
    pb::SearchFileMatch {
        path_ranges: f
            .path_ranges
            .iter()
            .map(|r| pb::SearchRange {
                start: at(r.start),
                end: at(r.end),
            })
            .collect(),
        path: f.path,
        blob_oid: f.oid.to_string(),
        chunks: f.chunks.into_iter().map(chunk_to_pb).collect(),
        symbols: f.symbols.into_iter().map(symbol_to_pb).collect(),
        score: f.score,
        owners: Vec::new(),
    }
}

fn indexed_file_to_pb(f: IndexedFile) -> pb::IndexedFileMatch {
    let at = |offset: usize| {
        Some(pb::SearchLocation {
            offset: to_u32(offset),
            line: 0,
            column: to_u32(offset),
        })
    };
    pb::IndexedFileMatch {
        repo_id: f.repo.0,
        branches: f
            .branches
            .into_iter()
            .map(|b| pb::IndexedBranch {
                name: b.name,
                commit_oid: b.commit,
            })
            .collect(),
        file: Some(pb::SearchFileMatch {
            path_ranges: f
                .path_ranges
                .iter()
                .map(|r| pb::SearchRange {
                    start: at(r.start),
                    end: at(r.end),
                })
                .collect(),
            path: f.path,
            blob_oid: sourcegull_gitstore::ObjectId::from(f.oid).to_string(),
            chunks: f.chunks.into_iter().map(chunk_to_pb).collect(),
            symbols: f.symbols.into_iter().map(symbol_to_pb).collect(),
            score: f.score,
            owners: Vec::new(),
        }),
    }
}

/// What an indexed search hands its forwarder ([`forward_files`]).
enum Forward {
    File(pb::IndexedFileMatch),
    /// The last message: the stats, or the error.
    End(Result<pb::SearchIndexedResponse, Status>),
}

/// Sends the files from `rx` as soon as the stream takes them, each message
/// with what has arrived by then (at most [`FILE_BATCH`]), then the end.
async fn forward_files(
    mut rx: mpsc::Receiver<Forward>,
    tx: mpsc::Sender<Result<pb::SearchIndexedResponse, Status>>,
) {
    while let Some(item) = rx.recv().await {
        let mut files = Vec::new();
        let mut end = None;
        let mut next = Some(item);
        while let Some(item) = next.take() {
            match item {
                Forward::File(f) => files.push(f),
                Forward::End(e) => end = Some(e),
            }
            if files.len() < FILE_BATCH && end.is_none() {
                next = rx.try_recv().ok();
            }
        }
        if !files.is_empty() {
            let msg = pb::SearchIndexedResponse {
                files,
                ..Default::default()
            };
            if tx.send(Ok(msg)).await.is_err() {
                // Dropping `rx` stops the search.
                return;
            }
        }
        if let Some(end) = end {
            let _ = tx.send(end).await;
            return;
        }
    }
}

fn index_stats_to_pb(s: &IndexSearchStats) -> pb::SearchStats {
    pb::SearchStats {
        files_considered: to_u32(s.files_considered),
        files_searched: to_u32(s.files_searched),
        files_skipped: to_u32(s.files_skipped),
        limit_hit: s.limit_hit,
        canceled: s.canceled,
        ..pb::SearchStats::default()
    }
}

/// Splits the requested branches into those `index` holds and those it
/// does not.
fn split_indexed(
    index: &dyn SearchIndex,
    repos: Vec<pb::IndexedRepoBranches>,
) -> Result<(Vec<RepoBranches>, Vec<pb::IndexedRepoBranches>), Status> {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    for rb in repos {
        let repo = rb
            .repo
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let held = index.branches(RepoId(repo.id)).unwrap_or_default();
        let (has, lacks): (Vec<String>, Vec<String>) = rb
            .branches
            .into_iter()
            .partition(|b| held.iter().any(|h| &h.name == b));
        if !has.is_empty() {
            found.push(RepoBranches {
                repo: RepoId(repo.id),
                branches: has,
            });
        }
        if !lacks.is_empty() {
            missing.push(pb::IndexedRepoBranches {
                repo: Some(repo),
                branches: lacks,
            });
        }
    }
    Ok((found, missing))
}

/// The contributor check of `repo` for an indexed match; `None` when the repository is not
/// readable (no file passes).
fn contributor_check(
    store: &GitStore,
    owners: &OwnerSearch,
    repo: RepoId,
) -> Option<Arc<crate::owners::ContributorCheck>> {
    let r = store.repo(repo).ok()?;
    owners.contributor_check(store, repo.0, &r)
}

/// The ownership of `repo` at the hex `commit` for an indexed match; none when the
/// repository or commit cannot be read.
fn repo_owners_at(
    store: &GitStore,
    owners: &OwnerSearch,
    repo: RepoId,
    commit: &str,
) -> sourcegull_own::RepoOwners {
    let (Ok(r), Ok(oid)) = (store.repo(repo), read::parse_oid(commit)) else {
        return sourcegull_own::RepoOwners::default();
    };
    owners.repo_owners(&r, repo.0, oid)
}

fn stats_to_pb(s: SearchStats) -> pb::SearchStats {
    pb::SearchStats {
        files_considered: to_u32(s.files_considered),
        files_searched: to_u32(s.files_searched),
        files_skipped_large: to_u32(s.files_skipped_large),
        files_skipped_binary: to_u32(s.files_skipped_binary),
        limit_hit: s.limit_hit,
        files_skipped: 0,
        canceled: s.canceled,
    }
}

fn signature_to_pb(s: read::Signature) -> pb::Signature {
    pb::Signature {
        name: s.name,
        email: s.email,
        time: s.time,
        tz_offset_seconds: s.tz_offset_seconds,
    }
}

/// Byte ranges in `text` as wire ranges with lines and columns.
fn text_ranges_to_pb(text: &str, ranges: &[std::ops::Range<usize>]) -> Vec<pb::SearchRange> {
    chunk_matches(text.as_bytes(), ranges, None)
        .into_iter()
        .flat_map(|chunk| chunk.ranges)
        .map(|r| pb::SearchRange {
            start: Some(loc_to_pb(r.start)),
            end: Some(loc_to_pb(r.end)),
        })
        .collect()
}

fn diff_file_to_pb(f: DiffFileMatch) -> pb::SearchDiffFile {
    pb::SearchDiffFile {
        old_path: f.old_path.unwrap_or_default(),
        new_path: f.new_path.unwrap_or_default(),
        hunks: f
            .hunks
            .into_iter()
            .map(|h| pb::SearchDiffHunk {
                old_start: h.old_start,
                old_lines: h.old_lines,
                new_start: h.new_start,
                new_lines: h.new_lines,
                ranges: text_ranges_to_pb(&h.body, &h.ranges),
                body: h.body,
                section: h.section,
            })
            .collect(),
    }
}

fn commit_match_to_pb(m: CommitMatch) -> pb::SearchCommitMatch {
    let c = m.commit;
    let ranges = text_ranges_to_pb(&c.message, &m.message_ranges);
    pb::SearchCommitMatch {
        diff: m.diff.into_iter().map(diff_file_to_pb).collect(),
        commit: Some(pb::Commit {
            oid: c.oid.to_string(),
            parents: c.parents.iter().map(ToString::to_string).collect(),
            author: Some(signature_to_pb(c.author)),
            committer: Some(signature_to_pb(c.committer)),
            message: c.message,
            tree_oid: c.tree.to_string(),
        }),
        message_ranges: ranges,
    }
}

#[tonic::async_trait]
impl SearchService for SearchNodeService {
    type SearchCommitsStream = BoxStream<pb::SearchCommitsResponse>;

    async fn search_commits(
        &self,
        req: Request<pb::SearchCommitsRequest>,
    ) -> Result<Response<Self::SearchCommitsStream>, Status> {
        let cancel = call_cancel(&req);
        let req = req.into_inner();
        let repo = req
            .repo
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let commit = read::parse_oid(&req.commit_oid)?;
        let hide = req
            .hide_oids
            .iter()
            .map(|h| read::parse_oid(h))
            .collect::<Result<Vec<_>, _>>()?;
        let pattern = req
            .pattern
            .as_ref()
            .map(|p| Matcher::from_spec(&spec_from_pb(p), req.case_sensitive))
            .transpose()
            .map_err(invalid)?;
        let filters = CommitFilters::compile(
            &commit_filters_from_pb(req.filters.unwrap_or_default()),
            req.case_sensitive,
        )
        .map_err(invalid)?;
        let paths = req
            .paths
            .map(|p| PathFilter::compile(&path_filter_from_pb(p), req.case_sensitive))
            .transpose()
            .map_err(invalid)?;
        let repo_id = RepoId(repo.id);
        let mut opts = CommitSearchOptions {
            paths,
            diff: req.diff,
            limit: if req.limit == 0 {
                usize::MAX
            } else {
                req.limit as usize
            },
            diff_cache: Some((self.diffs.clone(), repo_id)),
            cancel: cancel.clone(),
            ..CommitSearchOptions::default()
        };
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel::<Result<pb::SearchCommitsResponse, Status>>(4);
        tokio::task::spawn_blocking(move || {
            let mut run = || -> Result<(), GitError> {
                let r = store.repo(repo_id)?;
                // Unknown hidden commits (e.g. gone after a force push) are ignored.
                opts.hide = hide
                    .iter()
                    .copied()
                    .filter(|h| r.find_commit(*h).is_ok())
                    .collect();
                opts.log = store.commit_log(repo_id).unwrap_or_else(|e| {
                    tracing::warn!(repo = %repo_id, error = %e, "commit log unreadable; walking git");
                    None
                });
                let mut buf = Vec::with_capacity(COMMIT_BATCH);
                let mut gone = false;
                let end = search_commits(&r, commit, &filters, pattern.as_ref(), &opts, |m| {
                    buf.push(commit_match_to_pb(m));
                    if buf.len() == COMMIT_BATCH {
                        let commits = std::mem::take(&mut buf);
                        let msg = pb::SearchCommitsResponse {
                            commits,
                            ..Default::default()
                        };
                        if tx.blocking_send(Ok(msg)).is_err() {
                            gone = true;
                            return ControlFlow::Break(());
                        }
                    }
                    ControlFlow::Continue(())
                })?;
                if !gone {
                    let msg = pb::SearchCommitsResponse {
                        commits: buf,
                        limit_hit: end.limit_hit,
                        canceled: end.canceled,
                    };
                    let _ = tx.blocking_send(Ok(msg));
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = tx.blocking_send(Err(e.into()));
            }
        });
        Ok(Response::new(cancel_on_drop(rx, cancel.on_drop())))
    }

    type SearchIndexedStream = BoxStream<pb::SearchIndexedResponse>;

    async fn search_indexed(
        &self,
        req: Request<pb::SearchIndexedRequest>,
    ) -> Result<Response<Self::SearchIndexedStream>, Status> {
        let index = self
            .index
            .clone()
            .ok_or_else(|| Status::unimplemented("this node has no search index"))?;
        let cancel = call_cancel(&req);
        let req = req.into_inner();
        let (repos, missing) = split_indexed(&*index, req.repos)?;
        let owners = req
            .owners
            .map(OwnerSearch::from_pb)
            .filter(OwnerSearch::is_active);
        // With owner terms the index's candidates are filtered here, so the limit counts
        // the files that pass.
        let file_limit = req.file_limit as usize;
        let filtered = owners.as_ref().is_some_and(OwnerSearch::filters);
        let mut query = IndexQuery {
            repos,
            pattern: req.pattern.as_ref().map(spec_from_pb),
            path_pattern: req.path_pattern.as_ref().map(spec_from_pb),
            case_sensitive: req.case_sensitive,
            files: path_filter_from_pb(req.paths.unwrap_or_default()),
            file_limit: req.file_limit as usize,
            max_line_len: (req.max_line_len > 0).then_some(req.max_line_len as usize),
            symbols: req.symbols,
            bm25: req.bm25,
            repos_only: req.repos_only,
            flush_wall_time: (req.flush_ms > 0)
                .then(|| std::time::Duration::from_millis(u64::from(req.flush_ms))),
            cancel: cancel.clone(),
        };
        let repos_only = req.repos_only;
        if filtered || repos_only {
            query.file_limit = 0;
        }
        // Compile errors are the caller's, so report them before streaming.
        crate::CompiledQuery::compile(&query).map_err(invalid)?;
        let (tx, rx) = mpsc::channel::<Result<pb::SearchIndexedResponse, Status>>(4);
        let first = pb::SearchIndexedResponse {
            missing,
            ..Default::default()
        };
        if tx.try_send(Ok(first)).is_err() {
            return Err(Status::internal("search stream closed"));
        }
        // Defense in depth: the caller resolved `repos` against the viewer's
        // permissions, so a match from any other repository is a bug and is never sent.
        let requested: HashSet<RepoId> = query.repos.iter().map(|r| r.repo).collect();
        let store = self.store.clone();
        // Files go through a forwarder that sends what is ready, up to
        // FILE_BATCH per message: a burst (a ranked search's flush) is
        // batched, and a lone file is not held back waiting for more.
        let (ftx, frx) = mpsc::channel::<Forward>(FILE_BATCH * 4);
        tokio::spawn(forward_files(frx, tx));
        tokio::task::spawn_blocking(move || {
            let mut gone = false;
            let mut foreign = 0usize;
            let mut sent = 0usize;
            let mut owner_limit_hit = false;
            let mut at_commit = HashMap::new();
            let mut contributors = HashMap::new();
            // `repos_only`: the branches reported so far, so each is reported once whatever
            // the backend emits.
            let mut reported: HashSet<(RepoId, String)> = HashSet::new();
            let result = index.search(&query, &mut |mut f| {
                if !requested.contains(&f.repo) {
                    foreign += 1;
                    return ControlFlow::Continue(());
                }
                if repos_only {
                    f.branches
                        .retain(|b| reported.insert((f.repo, b.name.clone())));
                    if f.branches.is_empty() {
                        return ControlFlow::Continue(());
                    }
                    f.chunks.clear();
                    f.symbols.clear();
                }
                let mut file_owners = Vec::new();
                if let Some(o) = &owners {
                    // `file:has.contributor()` holds for the path on every branch.
                    if !o.contributors.is_empty() {
                        let check = contributors
                            .entry(f.repo)
                            .or_insert_with(|| contributor_check(&store, o, f.repo));
                        if !check.as_ref().is_some_and(|c| c.passes(&f.path)) {
                            return ControlFlow::Continue(());
                        }
                    }
                    // Keep the branches whose ownership passes.
                    let mut first = true;
                    f.branches.retain(|b| {
                        let ro = at_commit
                            .entry((f.repo, b.commit.clone()))
                            .or_insert_with(|| repo_owners_at(&store, o, f.repo, &b.commit));
                        let keep = ro.passes(&o.terms, &f.path);
                        if keep && first && o.report {
                            file_owners = ro.owner_texts(&f.path);
                            first = false;
                        }
                        keep
                    });
                    if f.branches.is_empty() {
                        return ControlFlow::Continue(());
                    }
                }
                let mut m = indexed_file_to_pb(f);
                if let Some(file) = m.file.as_mut() {
                    file.owners = file_owners;
                }
                if ftx.blocking_send(Forward::File(m)).is_err() {
                    gone = true;
                    return ControlFlow::Break(());
                }
                sent += 1;
                if filtered && file_limit > 0 && sent >= file_limit {
                    owner_limit_hit = true;
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            });
            if foreign > 0 {
                tracing::error!(
                    files = foreign,
                    "index returned matches from repositories outside the request; dropped"
                );
            }
            match result {
                Ok(_) if gone => {}
                Ok(mut stats) => {
                    stats.limit_hit |= owner_limit_hit;
                    let msg = pb::SearchIndexedResponse {
                        stats: Some(index_stats_to_pb(&stats)),
                        ..Default::default()
                    };
                    let _ = ftx.blocking_send(Forward::End(Ok(msg)));
                }
                Err(e) => {
                    let _ = ftx.blocking_send(Forward::End(Err(Status::internal(e.to_string()))));
                }
            }
        });
        Ok(Response::new(cancel_on_drop(rx, cancel.on_drop())))
    }

    async fn count_series(
        &self,
        req: Request<pb::CountSeriesRequest>,
    ) -> Result<Response<pb::CountSeriesResponse>, Status> {
        let cancel = call_cancel(&req);
        // The call's future is dropped when the client cancels it.
        let _guard = cancel.on_drop();
        let req = req.into_inner();
        let repo = req
            .repo
            .as_ref()
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let repo_id = RepoId(repo.id);
        let store = self.store.clone();
        let cache = self.series.clone();
        tokio::task::spawn_blocking(move || {
            let r = store.repo(repo_id)?;
            let log = store.commit_log(repo_id).unwrap_or_else(|e| {
                tracing::warn!(repo = %repo_id, error = %e, "commit log unreadable; walking git");
                None
            });
            crate::series::count_series_request(&r, log.as_ref(), req, &cache, cancel)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map(Response::new)
    }

    async fn language_stats(
        &self,
        req: Request<pb::LanguageStatsRequest>,
    ) -> Result<Response<pb::LanguageStatsResponse>, Status> {
        let cancel = call_cancel(&req);
        // The call's future is dropped when the client cancels it.
        let _guard = cancel.on_drop();
        let req = req.into_inner();
        let repo = req
            .repo
            .as_ref()
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let repo_id = RepoId(repo.id);
        let store = self.store.clone();
        let index = self.index.clone();
        let cache = self.loc.clone();
        tokio::task::spawn_blocking(move || {
            let r = store.repo(repo_id)?;
            crate::langstats::language_stats_request(
                &r,
                repo_id,
                &req.rev,
                index.as_deref(),
                &cache,
                &cancel,
            )
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map(Response::new)
    }

    async fn index_status(
        &self,
        req: Request<pb::IndexStatusRequest>,
    ) -> Result<Response<pb::IndexStatusResponse>, Status> {
        let repo = req
            .into_inner()
            .repo
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let repo_id = RepoId(repo.id);
        let queued = self.control.as_ref().is_some_and(|c| c.queued(repo_id));
        let Some(index) = self.index.clone() else {
            return Ok(Response::new(pb::IndexStatusResponse {
                queued,
                ..Default::default()
            }));
        };
        let status = tokio::task::spawn_blocking(move || index.repo_status(repo_id))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(index_status_to_pb(status, queued)))
    }

    async fn reindex(
        &self,
        req: Request<pb::ReindexRequest>,
    ) -> Result<Response<pb::ReindexResponse>, Status> {
        let repo = req
            .into_inner()
            .repo
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let control = self
            .control
            .as_ref()
            .filter(|_| self.index.is_some())
            .ok_or_else(|| Status::failed_precondition("this repo node keeps no search index"))?;
        control.reindex(RepoId(repo.id));
        Ok(Response::new(pb::ReindexResponse {}))
    }

    type SearchCommitStream = BoxStream<pb::SearchCommitResponse>;

    async fn search_commit(
        &self,
        req: Request<pb::SearchCommitRequest>,
    ) -> Result<Response<Self::SearchCommitStream>, Status> {
        let cancel = call_cancel(&req);
        let req = req.into_inner();
        let repo = req
            .repo
            .filter(|r| r.id > 0)
            .ok_or_else(|| Status::invalid_argument("a valid repo is required"))?;
        let commit = read::parse_oid(&req.commit_oid)?;
        let spec = req.pattern.as_ref().map(spec_from_pb);
        let matcher = spec
            .as_ref()
            .map(|p| Matcher::from_spec(p, req.case_sensitive))
            .transpose()
            .map_err(invalid)?;
        // Symbol search parses only files whose trigrams may match.
        let prefilter = req.symbols.then(|| {
            spec.as_ref()
                .map_or(TrigramQuery::All, |p| trigram_query(p, req.case_sensitive))
        });
        let mut paths = PathFilter::compile(
            &path_filter_from_pb(req.paths.unwrap_or_default()),
            req.case_sensitive,
        )
        .map_err(invalid)?;
        if let Some(p) = &req.path_pattern {
            paths = paths.with_pattern(
                Matcher::from_spec(&spec_from_pb(p), req.case_sensitive).map_err(invalid)?,
            );
        }
        let opts = UnindexedOptions {
            max_file_size: if req.max_file_size == 0 {
                DEFAULT_MAX_FILE_SIZE
            } else {
                req.max_file_size
            },
            max_line_len: (req.max_line_len > 0).then_some(req.max_line_len as usize),
            file_limit: if req.file_limit == 0 {
                usize::MAX
            } else {
                req.file_limit as usize
            },
            always_index: self
                .always_index
                .as_ref()
                .map(|rule| rule(&repo.name))
                .unwrap_or_default(),
            case_sensitive: req.case_sensitive,
            bm25: req.bm25,
            cancel: cancel.clone(),
            ..UnindexedOptions::default()
        };
        let owners = req
            .owners
            .map(OwnerSearch::from_pb)
            .filter(OwnerSearch::is_active);
        let store = self.store.clone();
        let symbols = self.symbols.clone();
        let (tx, rx) = mpsc::channel::<Result<pb::SearchCommitResponse, Status>>(4);
        tokio::task::spawn_blocking(move || {
            let run = || -> Result<(), GitError> {
                let r = store.repo(RepoId(repo.id))?;
                // Ownership at this commit: paths failing the terms are skipped
                // before their blobs are read.
                let ownership = owners
                    .as_ref()
                    .map(|o| (o, Arc::new(o.repo_owners(&r, repo.id, commit))));
                let paths = match &ownership {
                    Some((o, ro)) if o.filters() => paths.clone().with_owners(OwnerCheck {
                        owners: ro.clone(),
                        terms: o.terms.clone().into(),
                        contributors: o.contributor_check(&store, repo.id, &r),
                    }),
                    _ => paths.clone(),
                };
                let mut buf = Vec::with_capacity(FILE_BATCH);
                let mut gone = false;
                let emit = |f: FileMatch| {
                    let mut m = file_to_pb(f);
                    if let Some((o, ro)) = &ownership
                        && o.report
                    {
                        m.owners = ro.owner_texts(&m.path);
                    }
                    buf.push(m);
                    if buf.len() == FILE_BATCH {
                        let files = std::mem::take(&mut buf);
                        let msg = pb::SearchCommitResponse { files, stats: None };
                        if tx.blocking_send(Ok(msg)).is_err() {
                            gone = true;
                            return ControlFlow::Break(());
                        }
                    }
                    ControlFlow::Continue(())
                };
                let m = matcher.as_ref();
                let stats = match &prefilter {
                    Some(pre) => search_commit_symbols_cached(
                        &r,
                        commit,
                        m,
                        pre,
                        &paths,
                        &opts,
                        Some(&symbols),
                        emit,
                    )?,
                    None => search_commit(&r, commit, m, &paths, &opts, emit)?,
                };
                if !gone {
                    let msg = pb::SearchCommitResponse {
                        files: buf,
                        stats: Some(stats_to_pb(stats)),
                    };
                    let _ = tx.blocking_send(Ok(msg));
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = tx.blocking_send(Err(e.into()));
            }
        });
        Ok(Response::new(cancel_on_drop(rx, cancel.on_drop())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_gitstore::GitStoreConfig;
    use sourcegull_gitstore::Layout;
    use sourcegull_rpc::ErrorKind;
    use sourcegull_search::PatternSpec;
    use sourcegull_search_client::spec_to_pb;
    use tokio_stream::StreamExt;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn search_commit_streams_matches_and_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q"]);
        std::fs::write(work.join("a.txt"), "hello world\n").unwrap();
        std::fs::write(work.join("b.txt"), "bye\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "init"]);
        let head = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&work)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();

        let root = tmp.path().join("store");
        let store = GitStore::open(GitStoreConfig::new(&root)).unwrap();
        let dest = Layout::new(&root).repo_dir(RepoId(7));
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        );

        let svc = SearchNodeService::new(Arc::new(store));
        let req = pb::SearchCommitRequest {
            repo: Some(pb::RepoRef {
                id: 7,
                name: "r".into(),
            }),
            commit_oid: head.trim().into(),
            pattern: Some(spec_to_pb(&PatternSpec::Atom {
                regex: "WORLD".into(),
                negated: false,
            })),
            ..Default::default()
        };
        let mut stream = svc
            .search_commit(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        let mut files = Vec::new();
        let mut stats = None;
        while let Some(msg) = stream.next().await {
            let msg = msg.unwrap();
            files.extend(msg.files);
            stats = msg.stats.or(stats);
        }
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[0].chunks[0].content, "hello world");
        assert_eq!(stats.unwrap().files_searched, 2);

        // Calls whose deadline (`grpc-timeout`) has passed stop: commit search says it was
        // canceled, and language statistics fail with the timeout kind.
        let repo = Some(pb::RepoRef {
            id: 7,
            name: "r".into(),
        });
        let mut call = Request::new(pb::SearchCommitsRequest {
            repo: repo.clone(),
            commit_oid: head.trim().into(),
            ..Default::default()
        });
        call.set_timeout(std::time::Duration::ZERO);
        let mut stream = svc.search_commits(call).await.unwrap().into_inner();
        let last = stream.next().await.unwrap().unwrap();
        assert!(last.commits.is_empty() && last.canceled && last.limit_hit);
        let mut call = Request::new(pb::LanguageStatsRequest {
            repo,
            rev: String::new(),
        });
        call.set_timeout(std::time::Duration::ZERO);
        let err = svc.language_stats(call).await.unwrap_err();
        assert_eq!(ErrorKind::of(&err), Some(ErrorKind::Timeout));

        // No index: UNIMPLEMENTED, so `web` falls back to unindexed search.
        let req = pb::SearchIndexedRequest {
            repos: vec![pb::IndexedRepoBranches {
                repo: Some(pb::RepoRef {
                    id: 7,
                    name: "r".into(),
                }),
                branches: vec!["HEAD".into(), "dev".into()],
            }],
            pattern: Some(spec_to_pb(&PatternSpec::Atom {
                regex: "WORLD".into(),
                negated: false,
            })),
            ..Default::default()
        };
        let err = svc
            .search_indexed(Request::new(req.clone()))
            .await
            .err()
            .unwrap();
        assert_eq!(err.code(), tonic::Code::Unimplemented);

        // Indexed at HEAD only: "dev" is reported missing first, then the matches.
        let (index, _) =
            crate::ShardIndex::open(tmp.path().join("index"), Default::default()).unwrap();
        let index = Arc::new(index);
        let store = svc.store.clone();
        let git = store.repo(RepoId(7)).unwrap();
        let build = sourcegull_search::index::BuildRequest {
            repo: RepoId(7),
            name: "r".into(),
            git_dir: store.layout().repo_dir(RepoId(7)),
            branches: crate::branches_to_index(&git, &[]).unwrap(),
            ranks: None,
            always_index: Default::default(),
        };
        crate::update_index(&*index, &build).unwrap();
        let svc = SearchNodeService::new(store).with_index(index.clone());
        let mut stream = svc
            .search_indexed(Request::new(req.clone()))
            .await
            .unwrap()
            .into_inner();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.missing.len(), 1);
        assert_eq!(first.missing[0].branches, ["dev"]);
        let mut files = Vec::new();
        while let Some(msg) = stream.next().await {
            files.extend(msg.unwrap().files);
        }
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].branches[0].name, "HEAD");
        assert_eq!(files[0].branches[0].commit_oid, head.trim());
        assert_eq!(files[0].file.as_ref().unwrap().path, "a.txt");

        // Matches from repositories outside the request are dropped.
        let svc = SearchNodeService::new(svc.store.clone()).with_index(Arc::new(Leaky(index)));
        let mut stream = svc
            .search_indexed(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        let mut files = Vec::new();
        while let Some(msg) = stream.next().await {
            files.extend(msg.unwrap().files);
        }
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].repo_id, 7);
    }

    /// An index whose search runs until it is canceled, then reports its stats.
    struct UntilCanceled(std::sync::mpsc::Sender<IndexSearchStats>);

    impl SearchIndex for UntilCanceled {
        fn search(
            &self,
            query: &IndexQuery,
            _emit: &mut dyn FnMut(IndexedFile) -> ControlFlow<()>,
        ) -> Result<IndexSearchStats, sourcegull_search::index::IndexError> {
            let started = std::time::Instant::now();
            while !query.cancel.is_canceled() && started.elapsed().as_secs() < 30 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let stats = IndexSearchStats {
                canceled: query.cancel.is_canceled(),
                limit_hit: true,
                ..IndexSearchStats::default()
            };
            let _ = self.0.send(stats.clone());
            Ok(stats)
        }
        fn build(
            &self,
            _: &sourcegull_search::index::BuildRequest,
        ) -> Result<sourcegull_search::index::IndexedRepo, sourcegull_search::index::IndexError>
        {
            unimplemented!()
        }
        fn apply_delta(
            &self,
            _: &sourcegull_search::index::BuildRequest,
        ) -> Result<sourcegull_search::index::IndexedRepo, sourcegull_search::index::IndexError>
        {
            unimplemented!()
        }
        fn branches(&self, _: RepoId) -> Option<Vec<sourcegull_search::index::Branch>> {
            Some(vec![sourcegull_search::index::Branch {
                name: "HEAD".into(),
                commit: "0".repeat(40),
            }])
        }
        fn remove(&self, _: RepoId) -> Result<bool, sourcegull_search::index::IndexError> {
            Ok(false)
        }
        fn stats(&self) -> sourcegull_search::index::IndexStats {
            Default::default()
        }
    }

    #[tokio::test]
    async fn search_indexed_stops_when_the_call_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GitStore::open(GitStoreConfig::new(tmp.path())).unwrap();
        let (done, searched) = std::sync::mpsc::channel();
        let svc = SearchNodeService::new(Arc::new(store)).with_index(Arc::new(UntilCanceled(done)));
        let req = pb::SearchIndexedRequest {
            repos: vec![pb::IndexedRepoBranches {
                repo: Some(pb::RepoRef {
                    id: 7,
                    name: "r".into(),
                }),
                branches: vec!["HEAD".into()],
            }],
            ..Default::default()
        };
        let wait = std::time::Duration::from_secs(10);

        // The client cancels or goes away: the server drops the response stream.
        let mut stream = svc
            .search_indexed(Request::new(req.clone()))
            .await
            .unwrap()
            .into_inner();
        stream.next().await.unwrap().unwrap();
        drop(stream);
        let stats = tokio::task::spawn_blocking(move || {
            let stats = searched.recv_timeout(wait).unwrap();
            (stats, searched)
        })
        .await
        .unwrap();
        let (stats, searched) = stats;
        assert!(stats.canceled);

        // The call's deadline (`grpc-timeout`): the search ends, and the last message says
        // it was cut short.
        let mut call = Request::new(req);
        call.set_timeout(std::time::Duration::from_millis(50));
        let mut stream = svc.search_indexed(call).await.unwrap().into_inner();
        let mut last = None;
        while let Some(msg) = tokio::time::timeout(wait, stream.next()).await.unwrap() {
            last = msg.unwrap().stats.or(last);
        }
        let last = last.unwrap();
        assert!(last.canceled && last.limit_hit);
        assert!(searched.recv_timeout(wait).unwrap().canceled);
    }

    /// An index that also reports every match as coming from repository 99.
    struct Leaky(Arc<crate::ShardIndex>);

    impl SearchIndex for Leaky {
        fn search(
            &self,
            query: &IndexQuery,
            emit: &mut dyn FnMut(IndexedFile) -> ControlFlow<()>,
        ) -> Result<IndexSearchStats, sourcegull_search::index::IndexError> {
            self.0.search(query, &mut |f| {
                let mut other = f.clone();
                other.repo = RepoId(99);
                emit(other)?;
                emit(f)
            })
        }
        fn build(
            &self,
            req: &sourcegull_search::index::BuildRequest,
        ) -> Result<sourcegull_search::index::IndexedRepo, sourcegull_search::index::IndexError>
        {
            self.0.build(req)
        }
        fn apply_delta(
            &self,
            req: &sourcegull_search::index::BuildRequest,
        ) -> Result<sourcegull_search::index::IndexedRepo, sourcegull_search::index::IndexError>
        {
            self.0.apply_delta(req)
        }
        fn branches(&self, repo: RepoId) -> Option<Vec<sourcegull_search::index::Branch>> {
            self.0.branches(repo)
        }
        fn remove(&self, repo: RepoId) -> Result<bool, sourcegull_search::index::IndexError> {
            self.0.remove(repo)
        }
        fn stats(&self) -> sourcegull_search::index::IndexStats {
            self.0.stats()
        }
    }
}
