//! The indexed search engine's interface.
//!
//! Planning, ranking and streaming talk to an index only through
//! [`SearchIndex`] and the types here; none of them depends on a shard
//! format, so other backends can be plugged in. Calls are blocking (CPU and
//! mmap bound); async callers run them on a blocking pool. Results stream
//! through a callback that can stop the search, as in unindexed search.

use std::ops::{ControlFlow, Range};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use sourcegull_types::{AlwaysIndex, PathRanks, RepoId};

use crate::chunks::{ChunkMatch, SymbolMatch};
use crate::{CompileError, FileFilter, PatternSpec};

/// The most branches one repository can have indexed (`branch_mask` is a u64).
pub const MAX_BRANCHES: usize = 64;

/// The name the default branch is indexed and searched under, whatever it
/// is called in the repository.
pub const DEFAULT_BRANCH: &str = "HEAD";

/// How long a ranked search on a node holds its matches to send the best
/// first: then what it holds goes out best first, and later matches stream
/// as they are found. `web` closes its rank window once every node has sent
/// its first files.
pub const FLUSH_WALL_TIME: std::time::Duration = std::time::Duration::from_millis(100);

/// A branch and the commit it is indexed at (40 hex digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub commit: String,
}

/// Index one repository at the given branch heads, replacing what the index
/// held for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    pub repo: RepoId,
    pub name: String,
    /// The bare repository on this node.
    pub git_dir: PathBuf,
    /// The default branch first; at most [`MAX_BRANCHES`].
    pub branches: Vec<Branch>,
    /// The files' static ranks from the code graph; `None` when the
    /// repository has none. A build records their generation, and a
    /// different generation than the index holds means a full rebuild.
    pub ranks: Option<Arc<PathRanks>>,
    /// Files indexed with content above the size limit
    /// (`search.index.alwaysIndex`). A build under a different rule than the
    /// index holds is a full rebuild ([`SearchIndex::follows_always_index`]).
    pub always_index: AlwaysIndex,
}

impl BuildRequest {
    /// The generation of [`BuildRequest::ranks`] (0: none).
    pub fn ranks_generation(&self) -> u64 {
        self.ranks
            .as_ref()
            .map_or(0, |r| u64::try_from(r.generation).unwrap_or(0))
    }
}

/// What a build or delta left in the index for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRepo {
    pub repo: RepoId,
    pub branches: Vec<Branch>,
    pub docs: usize,
    /// Bytes of text content indexed (skipped files not counted).
    pub content_bytes: u64,
    /// Bytes on disk.
    pub index_bytes: u64,
}

/// A repository and the branches to search in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoBranches {
    pub repo: RepoId,
    pub branches: Vec<String>,
}

/// Stops a search before it ends: [`Cancel::cancel`] (the request is gone) or
/// a deadline. Searches check it between batches of documents, so a canceled
/// search stops within one batch. Clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// Also canceled once `deadline` passes.
    pub fn with_deadline(mut self, deadline: Option<Instant>) -> Cancel {
        self.deadline = match (self.deadline, deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Whether the search should stop: an atomic load, and the clock when
    /// there is a deadline.
    pub fn is_canceled(&self) -> bool {
        self.flag.load(Ordering::Relaxed) || self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// A guard that cancels when dropped, held by what the search reports to
    /// (a response stream), so the search stops when that is dropped.
    pub fn on_drop(&self) -> CancelOnDrop {
        CancelOnDrop(self.flag.clone())
    }
}

/// The same flag and deadline.
impl PartialEq for Cancel {
    fn eq(&self, other: &Cancel) -> bool {
        Arc::ptr_eq(&self.flag, &other.flag) && self.deadline == other.deadline
    }
}

impl Eq for Cancel {}

/// Cancels its [`Cancel`] when dropped ([`Cancel::on_drop`]).
#[derive(Debug)]
pub struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A content or path search over indexed branches.
#[derive(Debug, Clone, Default)]
pub struct IndexQuery {
    pub repos: Vec<RepoBranches>,
    /// Matched against file contents; `None` lists the files that pass the
    /// other filters.
    pub pattern: Option<PatternSpec>,
    /// Matched against file paths (path search).
    pub path_pattern: Option<PatternSpec>,
    pub case_sensitive: bool,
    pub files: FileFilter,
    /// Stop after this many matching files.
    pub file_limit: usize,
    pub max_line_len: Option<usize>,
    /// Symbol search: `pattern` matches the names of the symbols
    /// defined in a file instead of its content.
    pub symbols: bool,
    /// Rank with BM25 (keyword queries) instead of match quality.
    pub bm25: bool,
    /// Repository predicate sub-search (`repo:has.file()`): only which requested
    /// branches have a matching file matters. Backends may emit just the first match per
    /// repository and branch, unranked and without chunks, and skip the rest of a
    /// repository once all its branches matched; callers must not rely on more.
    pub repos_only: bool,
    /// How long a ranked search holds its matches before sending the best so far
    /// (`None`: [`FLUSH_WALL_TIME`]). `web` sets it so that every node's first page
    /// arrives by the same time after the search started (first event within 100 ms).
    pub flush_wall_time: Option<std::time::Duration>,
    /// Checked between batches of documents; a canceled search ends early with
    /// [`IndexSearchStats::canceled`] set.
    pub cancel: Cancel,
}

/// One matching file version.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedFile {
    pub repo: RepoId,
    /// The requested branches that hold this version of the file, with the
    /// commits they are indexed at.
    pub branches: Vec<Branch>,
    pub path: String,
    pub oid: [u8; 20],
    /// Empty for path-only results.
    pub chunks: Vec<ChunkMatch>,
    /// Byte ranges in `path` matched by the path pattern.
    pub path_ranges: Vec<Range<usize>>,
    /// Symbol search: the matching symbols.
    pub symbols: Vec<SymbolMatch>,
    /// The ranking score; 0 when the search has no pattern.
    pub score: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexSearchStats {
    /// Docs that passed the trigram and filter checks.
    pub files_considered: usize,
    /// Docs whose content was verified with the matcher.
    pub files_searched: usize,
    /// Docs not searchable by content (binary or too large).
    pub files_skipped: usize,
    /// The file limit or the caller stopped the search.
    pub limit_hit: bool,
    /// The search stopped at [`IndexQuery::cancel`] (also sets `limit_hit`).
    pub canceled: bool,
    /// Requested branches the index does not hold, for the caller to search
    /// unindexed.
    pub missing: Vec<(RepoId, String)>,
}

/// Lines and bytes of one language's text files ([`SearchIndex::language_stats`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageCount {
    /// The language id (`sourcegull_languages::Language::id`).
    pub lang: String,
    pub lines: u64,
    pub bytes: u64,
}

/// Totals over everything the index holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub repos: usize,
    pub docs: usize,
    pub content_bytes: u64,
    pub index_bytes: u64,
}

/// One file of a repository's index ([`RepoIndexStatus::shards`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardInfo {
    /// `base`, `compound` or `delta`.
    pub kind: String,
    /// File name in the index directory.
    pub file: String,
    /// Bytes on disk (a compound shard's whole file).
    pub size_bytes: u64,
    /// Docs of the repository in the file.
    pub docs: u64,
    /// Repositories in the file.
    pub repos: u32,
}

/// What the index holds for one repository, for the site admin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoIndexStatus {
    pub branches: Vec<Branch>,
    pub shards: Vec<ShardInfo>,
    /// Docs and text bytes searchable.
    pub docs: u64,
    pub content_bytes: u64,
    /// Bytes on disk (a compound shard's share by doc count).
    pub index_bytes: u64,
    /// Lines of the searchable text docs, and of those on the default branch.
    pub lines: u64,
    pub default_branch_lines: u64,
    /// When the newest of the repository's files was written.
    pub updated_at: Option<std::time::SystemTime>,
    pub ranks_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error("git: {0}")]
    Git(String),
    #[error("index I/O: {0}")]
    Io(String),
    #[error("corrupt index: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Invalid(String),
}

pub trait SearchIndex: Send + Sync {
    /// Searches the requested branches, calling `emit` per matching file
    /// until it breaks. Without a pattern, files come in request order of
    /// repos, then path order, up to the file limit. With one, results are
    /// ranked: each repository's best files by score, at most the
    /// file limit of them, and once that many were emitted only files that
    /// beat the worst of the best so far, so the best `file_limit` overall
    /// are among the files emitted.
    fn search(
        &self,
        query: &IndexQuery,
        emit: &mut dyn FnMut(IndexedFile) -> ControlFlow<()>,
    ) -> Result<IndexSearchStats, IndexError>;

    /// Indexes a repository from scratch and swaps it in.
    fn build(&self, req: &BuildRequest) -> Result<IndexedRepo, IndexError>;

    /// Moves an indexed repository to new branch heads. Implementations may
    /// rebuild fully; the result is the same as [`SearchIndex::build`].
    fn apply_delta(&self, req: &BuildRequest) -> Result<IndexedRepo, IndexError>;

    /// The branches indexed for `repo` and their commits; `None` when the
    /// repository is not indexed.
    fn branches(&self, repo: RepoId) -> Option<Vec<Branch>>;

    /// The generation of the static ranks indexed for `repo` (0: none);
    /// `None` when the repository is not indexed. Backends without ranks
    /// report 0.
    fn ranks_generation(&self, repo: RepoId) -> Option<u64> {
        self.branches(repo).map(|_| 0)
    }

    /// Whether the files of `repo` above the size limit have content
    /// exactly where `rule` asks for it, so a build under `rule` may be a
    /// delta or skipped. Backends without the limit report true.
    fn follows_always_index(&self, repo: RepoId, rule: &AlwaysIndex) -> bool {
        let _ = (repo, rule);
        true
    }

    /// Lines and bytes per language of the text files on `branch` of
    /// `repo`, from the per-doc metadata (Trends language statistics);
    /// `None` when the branch is not indexed. Backends without that
    /// metadata return `None` and callers count the tree instead.
    fn language_stats(&self, repo: RepoId, branch: &str) -> Option<Vec<LanguageCount>> {
        let _ = (repo, branch);
        None
    }

    /// Drops a repository from the index. Returns whether it was indexed.
    fn remove(&self, repo: RepoId) -> Result<bool, IndexError>;

    /// The repositories the index holds, so a reconcile pass can drop the ones gone from
    /// disk. Backends that cannot list them return none.
    fn indexed_repos(&self) -> Vec<RepoId> {
        Vec::new()
    }

    fn stats(&self) -> IndexStats;

    /// What the index holds for `repo`; `None` when it is not indexed.
    /// Backends without the detail report the branches only.
    fn repo_status(&self, repo: RepoId) -> Option<RepoIndexStatus> {
        self.branches(repo).map(|branches| RepoIndexStatus {
            branches,
            ..RepoIndexStatus::default()
        })
    }
}
