//! In-process reads through `gix`: revisions,
//! trees, blobs, commits and path history.
//!
//! All functions are synchronous and CPU/IO bound; async callers run them on
//! a blocking pool. Paths are repo-relative and `/`-separated; see
//! [`normalize_path`].

use std::collections::HashMap;
use std::ops::ControlFlow;

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::{EntryKind as GixEntryKind, EntryMode};

use crate::error::{GitError, Result};

/// Bytes inspected for NUL when deciding whether a blob is binary (as git does).
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// Normalizes a repo-relative path: strips leading/trailing `/`, collapses
/// `//` and `.`, and rejects `..`. The root is `""`.
pub fn normalize_path(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(GitError::InvalidArgument(format!(
                    "path {path:?} contains ..."
                )));
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

/// Resolves `rev` (empty means `HEAD`) to a commit id.
pub fn resolve_revision(repo: &gix::Repository, rev: &str) -> Result<ObjectId> {
    let spec = if rev.is_empty() { "HEAD" } else { rev };
    if spec.starts_with('-') || spec.contains("..") {
        return Err(GitError::RevisionNotFound(spec.to_owned()));
    }
    let not_found = |_| GitError::RevisionNotFound(spec.to_owned());
    let id = repo.rev_parse_single(spec).map_err(not_found)?;
    let commit = id
        .object()
        .map_err(not_found)?
        .peel_to_commit()
        .map_err(not_found)?;
    Ok(commit.id)
}

/// Finds a commit by id.
pub fn find_commit(repo: &gix::Repository, id: ObjectId) -> Result<gix::Commit<'_>> {
    repo.find_commit(id)
        .map_err(|_| GitError::RevisionNotFound(id.to_string()))
}

/// Parses a hex object id.
pub fn parse_oid(hex: &str) -> Result<ObjectId> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|_| GitError::InvalidArgument(format!("invalid object id {hex:?}")))
}

/// What a tree entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Executable file.
    Executable,
    /// Symbolic link (the blob holds the target).
    Symlink,
    /// Directory.
    Directory,
    /// Submodule (the oid is a commit in another repo).
    Submodule,
}

impl EntryKind {
    fn from_mode(mode: EntryMode) -> Self {
        match mode.kind() {
            GixEntryKind::Tree => EntryKind::Directory,
            GixEntryKind::Blob => EntryKind::File,
            GixEntryKind::BlobExecutable => EntryKind::Executable,
            GixEntryKind::Link => EntryKind::Symlink,
            GixEntryKind::Commit => EntryKind::Submodule,
        }
    }

    /// Whether the entry's object is a blob.
    pub fn is_blob(self) -> bool {
        matches!(
            self,
            EntryKind::File | EntryKind::Executable | EntryKind::Symlink
        )
    }
}

/// An entry in a tree listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Repo-relative path.
    pub path: String,
    /// Last path component.
    pub name: String,
    /// Kind of entry.
    pub kind: EntryKind,
    /// Git file mode (e.g. `0o100644`).
    pub mode: u32,
    /// Object id.
    pub oid: ObjectId,
    /// Blob size, when requested and the entry is a blob.
    pub size: Option<u64>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn blob_size(repo: &gix::Repository, oid: ObjectId) -> Result<u64> {
    Ok(repo.find_header(oid)?.size())
}

/// The entry at `path` in `commit`'s tree. The root (`""`) is a directory entry.
pub fn stat(repo: &gix::Repository, commit: ObjectId, path: &str) -> Result<TreeEntry> {
    let path = normalize_path(path)?;
    let tree = find_commit(repo, commit)?.tree()?;
    if path.is_empty() {
        return Ok(TreeEntry {
            path,
            name: String::new(),
            kind: EntryKind::Directory,
            mode: 0o040000,
            oid: tree.id,
            size: None,
        });
    }
    let entry = tree
        .lookup_entry(path.split('/'))?
        .ok_or_else(|| GitError::PathNotFound(path.clone()))?;
    let kind = EntryKind::from_mode(entry.mode());
    let oid = entry.object_id();
    Ok(TreeEntry {
        name: entry.filename().to_str_lossy().into_owned(),
        path,
        kind,
        mode: u32::from(entry.mode().value()),
        oid,
        size: if kind.is_blob() {
            Some(blob_size(repo, oid)?)
        } else {
            None
        },
    })
}

/// The object id at `path` in the tree `tree_id`, or `None` if absent.
fn path_oid(repo: &gix::Repository, tree_id: ObjectId, path: &str) -> Result<Option<ObjectId>> {
    if path.is_empty() {
        return Ok(Some(tree_id));
    }
    let tree = repo.find_tree(tree_id)?;
    Ok(tree.lookup_entry(path.split('/'))?.map(|e| e.object_id()))
}

/// Options for [`list_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListTreeOptions {
    /// Directory to list (`""` for the root).
    pub path: String,
    /// List subdirectories recursively (pre-order: a directory, then its contents).
    pub recursive: bool,
    /// Look up blob sizes (one object header read per blob).
    pub include_sizes: bool,
    /// Maximum number of entries.
    pub limit: usize,
}

impl Default for ListTreeOptions {
    fn default() -> Self {
        Self {
            path: String::new(),
            recursive: false,
            include_sizes: false,
            limit: 10_000,
        }
    }
}

/// Lists the directory `opts.path` at `commit`, calling `emit` per entry in
/// git tree order. Returns `true` if the listing stopped at `opts.limit`.
/// `emit` can stop early with [`ControlFlow::Break`].
pub fn list_tree(
    repo: &gix::Repository,
    commit: ObjectId,
    opts: &ListTreeOptions,
    emit: impl FnMut(TreeEntry) -> ControlFlow<()>,
) -> Result<bool> {
    list_tree_pruned(repo, commit, opts, |_| true, emit)
}

/// [`list_tree`] that descends (with `opts.recursive`) only into the
/// directories `descend` accepts; the directory entry itself is still emitted.
/// Skipped subtrees are never read.
pub fn list_tree_pruned(
    repo: &gix::Repository,
    commit: ObjectId,
    opts: &ListTreeOptions,
    mut descend: impl FnMut(&TreeEntry) -> bool,
    mut emit: impl FnMut(TreeEntry) -> ControlFlow<()>,
) -> Result<bool> {
    let dir = stat(repo, commit, &opts.path)?;
    if dir.kind != EntryKind::Directory {
        return Err(GitError::WrongEntryType {
            path: dir.path,
            expected: "directory",
        });
    }
    let mut count = 0usize;
    // Stack of (entries of a directory, next index) for an explicit DFS.
    let mut stack: Vec<(Vec<TreeEntry>, usize)> =
        vec![(read_dir(repo, &dir.path, dir.oid, opts.include_sizes)?, 0)];
    while let Some((entries, idx)) = stack.last_mut() {
        let Some(entry) = entries.get(*idx).cloned() else {
            stack.pop();
            continue;
        };
        *idx += 1;
        if count == opts.limit {
            return Ok(true);
        }
        count += 1;
        let down = opts.recursive && entry.kind == EntryKind::Directory && descend(&entry);
        let (sub_path, sub_oid) = (entry.path.clone(), entry.oid);
        if emit(entry).is_break() {
            return Ok(false);
        }
        if down {
            let children = read_dir(repo, &sub_path, sub_oid, opts.include_sizes)?;
            stack.push((children, 0));
        }
    }
    Ok(false)
}

/// Collects [`list_tree`] into a vector.
pub fn list_tree_vec(
    repo: &gix::Repository,
    commit: ObjectId,
    opts: &ListTreeOptions,
) -> Result<(Vec<TreeEntry>, bool)> {
    let mut out = Vec::new();
    let truncated = list_tree(repo, commit, opts, |e| {
        out.push(e);
        ControlFlow::Continue(())
    })?;
    Ok((out, truncated))
}

fn read_dir(
    repo: &gix::Repository,
    dir: &str,
    tree_id: ObjectId,
    include_sizes: bool,
) -> Result<Vec<TreeEntry>> {
    let tree = repo.find_tree(tree_id)?;
    let decoded = tree.decode()?;
    let mut out = Vec::with_capacity(decoded.entries.len());
    for e in &decoded.entries {
        let kind = EntryKind::from_mode(e.mode);
        let name = e.filename.to_str_lossy().into_owned();
        let oid = e.oid.to_owned();
        out.push(TreeEntry {
            path: join(dir, &name),
            name,
            kind,
            mode: u32::from(e.mode.value()),
            oid,
            size: if include_sizes && kind.is_blob() {
                Some(blob_size(repo, oid)?)
            } else {
                None
            },
        });
    }
    Ok(out)
}

/// File contents returned by [`read_blob`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Blob id (the content-address used for caching).
    pub oid: ObjectId,
    /// Full size in bytes.
    pub size: u64,
    /// Contents, cut at the limit.
    pub content: Vec<u8>,
    /// Whether a NUL byte appears in the first [`BINARY_SNIFF_BYTES`].
    pub binary: bool,
    /// Whether `content` was cut at the limit.
    pub truncated: bool,
}

/// Files larger than this (1 MiB) are not searched or indexed.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1 << 20;

/// Whether `data` looks binary (a NUL byte in the first 8000 bytes, as git does).
pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Reads the file at `path` in `commit`, returning at most `max_bytes` of content.
pub fn read_blob(
    repo: &gix::Repository,
    commit: ObjectId,
    path: &str,
    max_bytes: u64,
) -> Result<Blob> {
    let entry = stat(repo, commit, path)?;
    if !entry.kind.is_blob() {
        return Err(GitError::WrongEntryType {
            path: entry.path,
            expected: "file",
        });
    }
    read_blob_by_oid(repo, entry.oid, max_bytes)
}

/// Reads a blob by id, returning at most `max_bytes` of content.
pub fn read_blob_by_oid(repo: &gix::Repository, oid: ObjectId, max_bytes: u64) -> Result<Blob> {
    let mut blob = repo.find_blob(oid)?;
    let size = blob.data.len() as u64;
    let binary = is_binary(&blob.data);
    let mut content = blob.take_data();
    let truncated = size > max_bytes;
    if truncated {
        content.truncate(max_bytes as usize);
    }
    Ok(Blob {
        oid,
        size,
        content,
        binary,
        truncated,
    })
}

/// A person and time on a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Name.
    pub name: String,
    /// Email.
    pub email: String,
    /// Seconds since the epoch.
    pub time: i64,
    /// Offset from UTC in seconds.
    pub tz_offset_seconds: i32,
}

impl Signature {
    fn from_ref(s: gix::actor::SignatureRef<'_>) -> Self {
        let t = s.time().unwrap_or_default();
        let s = s.trim();
        Self {
            name: s.name.to_str_lossy().into_owned(),
            email: s.email.to_str_lossy().into_owned(),
            time: t.seconds,
            tz_offset_seconds: t.offset,
        }
    }
}

/// Commit metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Commit id.
    pub oid: ObjectId,
    /// Parent ids in order.
    pub parents: Vec<ObjectId>,
    /// Author.
    pub author: Signature,
    /// Committer.
    pub committer: Signature,
    /// Full message (lossily decoded as UTF-8).
    pub message: String,
    /// Root tree id.
    pub tree: ObjectId,
}

impl CommitInfo {
    /// The first line of the message.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or_default()
    }
}

/// Reads commit metadata.
pub fn commit_info(repo: &gix::Repository, id: ObjectId) -> Result<CommitInfo> {
    let commit = find_commit(repo, id)?;
    commit_to_info(&commit)
}

fn commit_to_info(commit: &gix::Commit<'_>) -> Result<CommitInfo> {
    let c = commit.decode()?;
    Ok(CommitInfo {
        oid: commit.id,
        parents: c.parents().collect(),
        author: Signature::from_ref(c.author()?),
        committer: Signature::from_ref(c.committer()?),
        message: c.message.to_str_lossy().into_owned(),
        tree: c.tree(),
    })
}

/// Options for [`log`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogOptions {
    /// Only commits that changed this path (file or directory); `""` for all.
    pub path: String,
    /// Matching commits to skip (paging).
    pub skip: usize,
    /// Maximum commits to return.
    pub limit: usize,
    /// Follow only first parents.
    pub first_parent: bool,
    /// Leave out these commits and their ancestors (`start ^hide…`).
    pub hide: Vec<ObjectId>,
    /// Only commits committed at or before this time (seconds since the epoch), as
    /// `git log --before`.
    pub before: Option<i64>,
    /// Only commits committed at or after this time (seconds since the epoch), as
    /// `git log --after`: the walk (newest first by committer time) stops at the first
    /// commit committed before it.
    pub after: Option<i64>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            path: String::new(),
            skip: 0,
            limit: 100,
            first_parent: false,
            hide: Vec::new(),
            before: None,
            after: None,
        }
    }
}

/// A page of [`log`] results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogPage {
    /// Commits, newest first.
    pub commits: Vec<CommitInfo>,
    /// Whether more matching commits follow.
    pub has_more: bool,
}

/// Commit history from `start`, newest first by commit time.
///
/// With a path, a commit matches if the path's object differs from every
/// parent's (a merge that took the path unchanged from one side is skipped);
/// root commits match if they contain the path. This equals `git log -- path`
/// on linear history and on merges; unlike git's default history
/// simplification, side branches are still walked.
pub fn log(repo: &gix::Repository, start: ObjectId, opts: &LogOptions) -> Result<LogPage> {
    let mut commits = Vec::new();
    let has_more = log_each(repo, start, opts, |c| {
        commits.push(c);
        ControlFlow::Continue(())
    })?;
    Ok(LogPage { commits, has_more })
}

/// Streaming form of [`log`]: calls `emit` for each commit of the page and
/// returns whether more matching commits follow.
pub fn log_each(
    repo: &gix::Repository,
    start: ObjectId,
    opts: &LogOptions,
    mut emit: impl FnMut(CommitInfo) -> ControlFlow<()>,
) -> Result<bool> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;

    let path = normalize_path(&opts.path)?;
    find_commit(repo, start)?;
    let mut walk = repo
        .rev_walk([start])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .use_commit_graph(true);
    if opts.first_parent {
        walk = walk.first_parent_only();
    }
    if !opts.hide.is_empty() {
        for &h in &opts.hide {
            find_commit(repo, h)?;
        }
        walk = walk.with_hidden(opts.hide.iter().copied());
    }
    // Path object per commit, so a commit's lookup is reused as its children's parent.
    let mut path_cache: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
    let mut lookup = |repo: &gix::Repository, commit: ObjectId| -> Result<Option<ObjectId>> {
        if let Some(v) = path_cache.get(&commit) {
            return Ok(*v);
        }
        let tree = find_commit(repo, commit)?.tree_id()?.detach();
        let v = path_oid(repo, tree, &path)?;
        if path_cache.len() > 100_000 {
            path_cache.clear();
        }
        path_cache.insert(commit, v);
        Ok(v)
    };

    let mut skipped = 0usize;
    let mut emitted = 0usize;
    for info in walk.all()? {
        let info = info.map_err(GitError::gix)?;
        let id = info.id;
        if opts.before.is_some() || opts.after.is_some() {
            let time = match info.commit_time {
                Some(t) => t,
                None => commit_to_info(&info.object()?)?.committer.time,
            };
            if opts.after.is_some_and(|after| time < after) {
                // Newest first: every commit left is older.
                break;
            }
            if opts.before.is_some_and(|before| time > before) {
                continue;
            }
        }
        if !path.is_empty() {
            let own = lookup(repo, id)?;
            let parents: Vec<ObjectId> = if opts.first_parent {
                info.parent_ids().take(1).map(|p| p.detach()).collect()
            } else {
                info.parent_ids().map(|p| p.detach()).collect()
            };
            let matches = if parents.is_empty() {
                own.is_some()
            } else {
                let mut differs_from_all = true;
                for p in parents {
                    if lookup(repo, p)? == own {
                        differs_from_all = false;
                        break;
                    }
                }
                differs_from_all
            };
            if !matches {
                continue;
            }
        }
        if skipped < opts.skip {
            skipped += 1;
            continue;
        }
        if emitted == opts.limit {
            return Ok(true);
        }
        emitted += 1;
        let commit = info.object()?;
        if emit(commit_to_info(&commit)?).is_break() {
            return Ok(false);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("").unwrap(), "");
        assert_eq!(normalize_path("/").unwrap(), "");
        assert_eq!(normalize_path("/a//b/./c/").unwrap(), "a/b/c");
        assert!(matches!(
            normalize_path("a/../b"),
            Err(GitError::InvalidArgument(_))
        ));
    }

    #[test]
    fn detects_binary() {
        assert!(!is_binary(b"hello\nworld"));
        assert!(is_binary(b"PNG\0\x01"));
        let mut late = vec![b'a'; BINARY_SNIFF_BYTES + 10];
        late[BINARY_SNIFF_BYTES + 5] = 0;
        assert!(!is_binary(&late));
    }
}
