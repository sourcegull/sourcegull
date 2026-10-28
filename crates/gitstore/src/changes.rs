//! Changed paths per commit, a sidecar of the commit
//! metadata log ([`crate::commitlog`]).
//!
//! `<repo>.git/sourcegull/changes.log` holds one record per commit of
//! `commits.log`, in the same order: the files the commit changed against its
//! first parent (the empty tree for a root commit; none for a merge, as
//! `git log` shows them). The Code Ownership signals read it: the recent
//! contributors of a path ([`ChangeLog::changed_paths`] over the commits of
//! the last 90 days) and the contributors `file:has.contributor()` matches
//! ([`ContributorIndex`]), without diffing trees at query time.
//!
//! Paths are interned: a record first lists the paths it introduces (their
//! ids follow the ones before), then the ids it changed. Like the commit log,
//! records are `u32 len`, `u32 crc32` of the payload, then the payload
//! (little-endian), and the file is only appended to, or replaced by a
//! rename:
//!
//! ```text
//! oid[20]
//! nnew:u32, (len:u32 path) × nnew      paths first seen in this record
//! n:u32, id:u32 × n                    the changed paths
//! ```
//!
//! The file is extended to the commit log's length after each clone and fetch
//! (in the background) and on read ([`ChangeLogs::current`]), diffing only the
//! commits it lacks. A record's oid must equal the commit log's at the same
//! position; otherwise the file is rebuilt.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::object::tree::diff::ChangeDetached;
use sourcegull_types::RepoId;

use crate::commitlog::CommitLog;
use crate::error::{GitError, Result};

const MAGIC: &[u8; 8] = b"SKCHG001";
const OID: usize = 20;

/// The changes file of the bare repo at `git_dir`.
pub fn changes_path(git_dir: &Path) -> PathBuf {
    git_dir.join("sourcegull").join("changes.log")
}

/// A loaded changes file.
pub struct ChangeLog {
    paths: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
    /// Start of each commit's ids in `changed` (one more entry than commits).
    starts: Vec<u32>,
    changed: Vec<u32>,
    last_oid: Option<ObjectId>,
    /// Bytes covered by valid records.
    valid_len: u64,
    file_len: u64,
    dev_ino: (u64, u64),
    mtime: (i64, i64),
    contributors: OnceLock<ContributorIndex>,
}

impl std::fmt::Debug for ChangeLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeLog")
            .field("commits", &self.len())
            .field("paths", &self.paths.len())
            .finish_non_exhaustive()
    }
}

impl ChangeLog {
    fn empty() -> Self {
        ChangeLog {
            paths: Vec::new(),
            ids: HashMap::new(),
            starts: vec![0],
            changed: Vec::new(),
            last_oid: None,
            valid_len: MAGIC.len() as u64,
            file_len: 0,
            dev_ino: (0, 0),
            mtime: (0, 0),
            contributors: OnceLock::new(),
        }
    }

    /// Reads the file at `path`. `Ok(None)` when there is none; a file with
    /// another header is an error.
    pub fn open(path: &Path) -> Result<Option<ChangeLog>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = std::fs::metadata(path)?;
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(GitError::Corrupt(format!(
                "{}: not a changes log",
                path.display()
            )));
        }
        let mut log = ChangeLog::empty();
        log.file_len = bytes.len() as u64;
        log.dev_ino = (meta.dev(), meta.ino());
        log.mtime = (meta.mtime(), meta.mtime_nsec());
        let mut at = MAGIC.len();
        while let Some((payload, next)) = record_at(&bytes, at) {
            if !log.push_record(payload) {
                break;
            }
            at = next;
        }
        log.valid_len = at as u64;
        Ok(Some(log))
    }

    /// Adds a record's paths and ids; `false` (and nothing added) when it is
    /// malformed.
    fn push_record(&mut self, p: &[u8]) -> bool {
        let Some(parsed) = parse_payload(p, self.paths.len()) else {
            return false;
        };
        for s in parsed.new_paths {
            let s: Box<str> = s.into();
            self.ids.insert(s.clone(), self.paths.len() as u32);
            self.paths.push(s);
        }
        self.changed.extend(parsed.ids);
        self.starts.push(self.changed.len() as u32);
        self.last_oid = Some(parsed.oid);
        true
    }

    /// Commits covered (positions `0..len()` of the commit log).
    pub fn len(&self) -> usize {
        self.starts.len() - 1
    }

    /// Whether no commit is covered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Distinct paths.
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// The id of `path`, if any commit changed it.
    pub fn path_id(&self, path: &str) -> Option<u32> {
        self.ids.get(path).copied()
    }

    /// The path with id `id`.
    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    /// The ids of the files the commit at `pos` changed (`pos < len()`).
    pub fn changed_ids(&self, pos: u32) -> &[u32] {
        let (a, b) = (
            self.starts[pos as usize] as usize,
            self.starts[pos as usize + 1] as usize,
        );
        &self.changed[a..b]
    }

    /// The files the commit at `pos` changed, in path order.
    pub fn changed_paths(&self, pos: u32) -> impl Iterator<Item = &str> + '_ {
        self.changed_ids(pos).iter().map(|&i| self.path(i))
    }

    fn has_torn_tail(&self) -> bool {
        self.valid_len < self.file_len
    }

    /// Whether the records match the start of `log`.
    fn aligned_with(&self, log: &CommitLog) -> bool {
        match self.len() {
            0 => true,
            n if n > log.len() => false,
            n => self.last_oid == Some(log.get(n as u32 - 1).oid()),
        }
    }

    /// The contributors of every path, built on first use from `log` (which
    /// must cover at least the commits of this file).
    pub fn contributors(&self, log: &CommitLog) -> &ContributorIndex {
        self.contributors
            .get_or_init(|| ContributorIndex::build(self, log))
    }

    /// Approximate memory held.
    pub fn memory_bytes(&self) -> usize {
        self.paths.iter().map(|p| p.len() * 2 + 48).sum::<usize>()
            + self.starts.len() * 4
            + self.changed.len() * 4
    }
}

struct Parsed<'a> {
    oid: ObjectId,
    new_paths: Vec<&'a str>,
    ids: Vec<u32>,
}

/// Parses a payload; `known` is the number of paths before it.
fn parse_payload(p: &[u8], known: usize) -> Option<Parsed<'_>> {
    let oid = ObjectId::from_bytes_or_panic(p.get(..OID)?);
    let mut at = OID;
    let u32_next = |at: &mut usize| -> Option<u32> {
        let v = u32::from_le_bytes(p.get(*at..*at + 4)?.try_into().ok()?);
        *at += 4;
        Some(v)
    };
    let nnew = u32_next(&mut at)? as usize;
    let mut new_paths = Vec::with_capacity(nnew.min(4096));
    for _ in 0..nnew {
        let len = u32_next(&mut at)? as usize;
        let s = std::str::from_utf8(p.get(at..at.checked_add(len)?)?).ok()?;
        new_paths.push(s);
        at += len;
    }
    let n = u32_next(&mut at)? as usize;
    let total = known + nnew;
    let mut ids = Vec::with_capacity(n.min(65536));
    for _ in 0..n {
        let id = u32_next(&mut at)?;
        if id as usize >= total {
            return None;
        }
        ids.push(id);
    }
    (at == p.len()).then_some(Parsed {
        oid,
        new_paths,
        ids,
    })
}

fn record_at(bytes: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let header = bytes.get(at..at + 8)?;
    let len = u32::from_le_bytes(header[..4].try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(header[4..].try_into().ok()?);
    let payload = bytes.get(at + 8..(at + 8).checked_add(len)?)?;
    (crc32fast::hash(payload) == crc).then_some((payload, at + 8 + len))
}

/// The files `commit` changed against its first parent (whose tree is
/// `parent_tree`, `None` for a root commit), in path order. Trees,
/// submodules and renames are not followed: a rename is a deletion and an
/// addition, as `git log --no-renames` shows it.
pub fn changed_files(
    repo: &gix::Repository,
    tree: ObjectId,
    parent_tree: Option<ObjectId>,
) -> Result<Vec<String>> {
    let new_tree = repo.find_tree(tree)?;
    let old_tree = match parent_tree {
        Some(t) => repo.find_tree(t)?,
        None => repo.empty_tree(),
    };
    let options = gix::diff::Options::default().with_rewrites(None);
    let changes = repo.diff_tree_to_tree(&old_tree, &new_tree, options)?;
    let mut out: Vec<String> = changes
        .iter()
        .filter_map(|c| {
            let (location, mode) = match c {
                ChangeDetached::Addition {
                    location,
                    entry_mode,
                    ..
                }
                | ChangeDetached::Deletion {
                    location,
                    entry_mode,
                    ..
                }
                | ChangeDetached::Modification {
                    location,
                    entry_mode,
                    ..
                } => (location, entry_mode),
                ChangeDetached::Rewrite {
                    location,
                    entry_mode,
                    ..
                } => (location, entry_mode),
            };
            (!mode.is_tree() && !mode.is_commit()).then(|| location.to_str_lossy().into_owned())
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The tree of the first parent of the commit at `pos`, or `None` for a root
/// commit. A parent missing from the log is read from git.
fn first_parent_tree(
    repo: &gix::Repository,
    log: &CommitLog,
    pos: u32,
) -> Result<Option<ObjectId>> {
    let c = log.get(pos);
    match c.parents().next() {
        None => Ok(None),
        Some(Some(p)) => Ok(Some(log.get(p).tree())),
        Some(None) => {
            let commit = repo.find_commit(c.oid())?;
            let Some(parent) = commit.parent_ids().next() else {
                return Ok(None);
            };
            Ok(Some(repo.find_commit(parent.detach())?.tree_id()?.detach()))
        }
    }
}

/// What [`update`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Commits appended.
    pub added: usize,
    /// Commits covered afterwards.
    pub total: usize,
    /// Whether the file was written anew.
    pub rewritten: bool,
}

/// Commits missing from the file above which [`update`] lists changes with
/// `git log --raw` (one process, about 30 times faster than diffing trees with
/// gix on golang/go) instead of diffing in process.
pub const GIT_BATCH_MIN: usize = 64;

/// Extends the changes file of `repo` to cover `log`, diffing the commits it
/// lacks; rebuilds it when it does not match `log`. With `git_bin`, more than
/// [`GIT_BATCH_MIN`] missing commits are listed by git. One writer per repo at
/// a time (callers hold [`ChangeLogs`]' per-repo lock).
pub fn update(
    repo: &gix::Repository,
    log: &CommitLog,
    current: Option<&ChangeLog>,
    git_bin: Option<&Path>,
) -> Result<UpdateStats> {
    update_with(repo, log, current, git_bin, GIT_BATCH_MIN)
}

fn update_with(
    repo: &gix::Repository,
    log: &CommitLog,
    current: Option<&ChangeLog>,
    git_bin: Option<&Path>,
    git_min: usize,
) -> Result<UpdateStats> {
    let path = changes_path(repo.git_dir());
    let loaded;
    let current = match current {
        Some(c) => Some(c),
        None => {
            loaded = ChangeLog::open(&path).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "rebuilding the changes log");
                None
            });
            loaded.as_ref()
        }
    };
    let current = current.filter(|c| c.aligned_with(log));
    let base = current.map_or(0, ChangeLog::len);
    let rewrite = current.is_none_or(ChangeLog::has_torn_tail);
    if base == log.len() && !rewrite {
        return Ok(UpdateStats {
            added: 0,
            total: base,
            rewritten: false,
        });
    }

    // Consecutive commits share most trees: cache objects across the diffs.
    let mut cached = repo.clone();
    cached.object_cache_size_if_unset(64 * 1024 * 1024);
    let repo = &cached;
    let mut known_paths = current.map_or(0, ChangeLog::path_count) as u32;
    let mut new_ids: HashMap<String, u32> = HashMap::new();
    let lookup = |p: &str, new_ids: &HashMap<String, u32>| {
        current
            .and_then(|c| c.path_id(p))
            .or_else(|| new_ids.get(p).copied())
    };
    let mut out = Vec::new();
    let mut add = |pos: u32, files: Vec<String>| {
        let c = log.get(pos);
        let mut fresh: Vec<&str> = Vec::new();
        let mut ids = Vec::with_capacity(files.len());
        for f in &files {
            let id = match lookup(f, &new_ids) {
                Some(id) => id,
                None => {
                    let id = known_paths;
                    known_paths += 1;
                    new_ids.insert(f.clone(), id);
                    fresh.push(f);
                    id
                }
            };
            ids.push(id);
        }
        encode(&mut out, &c.oid(), &fresh, &ids);
    };
    let range = base as u32..log.len() as u32;
    match git_bin {
        Some(git) if range.len() > git_min => {
            let mut next = range.start;
            git_changed_files(git, repo.git_dir(), log, range.clone(), |oid, files| {
                if log.get(next).oid() != oid {
                    return Err(GitError::Corrupt(format!(
                        "git log listed {oid}, expected {}",
                        log.get(next).oid()
                    )));
                }
                add(next, files);
                next += 1;
                Ok(())
            })?;
            if next != range.end {
                return Err(GitError::Corrupt(format!(
                    "git log listed {} of {} commits",
                    next - range.start,
                    range.len()
                )));
            }
        }
        _ => {
            for pos in range {
                let c = log.get(pos);
                let files = if c.is_merge() {
                    Vec::new()
                } else {
                    changed_files(repo, c.tree(), first_parent_tree(repo, log, pos)?)?
                };
                add(pos, files);
            }
        }
    }

    if rewrite {
        let dir = path.parent().expect("changes path has a parent");
        std::fs::create_dir_all(dir)?;
        let tmp = tempfile::NamedTempFile::new_in(dir)?;
        {
            let mut w = std::io::BufWriter::new(tmp.as_file());
            w.write_all(MAGIC)?;
            if let Some(c) = current {
                // Keep the valid records of the current file.
                let bytes = std::fs::read(&path)?;
                w.write_all(&bytes[MAGIC.len()..c.valid_len as usize])?;
            }
            w.write_all(&out)?;
            w.flush()?;
        }
        tmp.as_file().sync_data()?;
        tmp.persist(&path).map_err(|e| GitError::Io(e.error))?;
    } else {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
        f.write_all(&out)?;
        f.sync_data()?;
    }
    Ok(UpdateStats {
        added: log.len() - base,
        total: log.len(),
        rewritten: rewrite,
    })
}

/// Lists the files each commit at `range` changed with one `git log --raw`
/// (the commits fed on stdin, in log order), calling `each` per commit in that
/// order. Merges list nothing, as without `-m`; root commits list their files.
fn git_changed_files(
    git_bin: &Path,
    git_dir: &Path,
    log: &CommitLog,
    range: std::ops::Range<u32>,
    mut each: impl FnMut(ObjectId, Vec<String>) -> Result<()>,
) -> Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let mut child = Command::new(git_bin)
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "-c",
            "core.quotePath=false",
            "log",
            "--no-walk=unsorted",
            "--stdin",
            "--raw",
            "--no-renames",
            "--no-abbrev",
            "--no-ext-diff",
            "--root",
            "-z",
            "--format=%H",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let oids: Vec<ObjectId> = range.map(|p| log.get(p).oid()).collect();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut w = std::io::BufWriter::new(&mut stdin);
        for oid in oids {
            writeln!(w, "{oid}")?;
        }
        w.flush()
    });
    let mut reader = BufReader::with_capacity(1 << 20, child.stdout.take().expect("piped stdout"));
    let mut current: Option<(ObjectId, Vec<String>)> = None;
    // The mode that decides whether a raw entry is a file: the new one, or the old
    // one for a deletion.
    let mut pending_mode: Option<bool> = None;
    let mut token = Vec::new();
    let mut flush = |current: &mut Option<(ObjectId, Vec<String>)>| -> Result<()> {
        if let Some((oid, mut files)) = current.take() {
            files.sort_unstable();
            files.dedup();
            each(oid, files)?;
        }
        Ok(())
    };
    loop {
        token.clear();
        if reader.read_until(0, &mut token)? == 0 {
            break;
        }
        if token.last() == Some(&0) {
            token.pop();
        }
        if let Some(is_file) = pending_mode.take() {
            if is_file && let Some((_, files)) = current.as_mut() {
                files.push(String::from_utf8_lossy(&token).into_owned());
            }
            continue;
        }
        for piece in token.split(|&b| b == b'\n').filter(|p| !p.is_empty()) {
            if piece.first() == Some(&b':') {
                // `:old_mode new_mode old_oid new_oid status`
                let meta = String::from_utf8_lossy(piece);
                let mut f = meta[1..].split(' ');
                let (old, new) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
                let mode = if new.trim_start_matches('0').is_empty() {
                    old
                } else {
                    new
                };
                pending_mode = Some(!mode.starts_with("16") && !mode.starts_with("04"));
            } else if let Ok(oid) = ObjectId::from_hex(piece) {
                flush(&mut current)?;
                current = Some((oid, Vec::new()));
            }
        }
    }
    flush(&mut current)?;
    let status = child.wait()?;
    writer
        .join()
        .map_err(|_| GitError::Corrupt("git log stdin writer panicked".into()))??;
    if !status.success() {
        return Err(GitError::Corrupt(format!("git log --raw failed: {status}")));
    }
    Ok(())
}

fn encode(out: &mut Vec<u8>, oid: &ObjectId, fresh: &[&str], ids: &[u32]) {
    let start = out.len();
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(oid.as_bytes());
    out.extend_from_slice(&(fresh.len() as u32).to_le_bytes());
    for p in fresh {
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    let payload = &out[start + 8..];
    let len = (payload.len() as u32).to_le_bytes();
    let crc = crc32fast::hash(payload).to_le_bytes();
    out[start..start + 4].copy_from_slice(&len);
    out[start + 4..start + 8].copy_from_slice(&crc);
}

/// The authors who changed each path over the whole history in the log
/// (`file:has.contributor()`).
pub struct ContributorIndex {
    /// Distinct authors as `name <email>`, with the name's length.
    authors: Vec<(Box<str>, usize)>,
    /// Per path id: sorted author ids.
    by_path: Vec<Vec<u32>>,
}

impl std::fmt::Debug for ContributorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContributorIndex")
            .field("authors", &self.authors.len())
            .field("paths", &self.by_path.len())
            .finish()
    }
}

impl ContributorIndex {
    fn build(changes: &ChangeLog, log: &CommitLog) -> Self {
        let mut authors: Vec<(Box<str>, usize)> = Vec::new();
        let mut author_ids: HashMap<&str, u32> = HashMap::new();
        let mut by_path: Vec<Vec<u32>> = vec![Vec::new(); changes.path_count()];
        for pos in 0..changes.len().min(log.len()) as u32 {
            let ids = changes.changed_ids(pos);
            if ids.is_empty() {
                continue;
            }
            let a = log.get(pos).author();
            let id = *author_ids.entry(a.ident).or_insert_with(|| {
                authors.push((a.ident.into(), a.name().len()));
                (authors.len() - 1) as u32
            });
            for &p in ids {
                let list = &mut by_path[p as usize];
                if list.last() != Some(&id) {
                    list.push(id);
                }
            }
        }
        for list in &mut by_path {
            list.sort_unstable();
            list.dedup();
        }
        ContributorIndex { authors, by_path }
    }

    /// Distinct authors.
    pub fn author_count(&self) -> usize {
        self.authors.len()
    }

    /// The name and email of author `id`.
    pub fn author(&self, id: u32) -> (&str, &str) {
        let (ident, name_len) = &self.authors[id as usize];
        let email = ident
            .get(name_len + 2..ident.len().saturating_sub(1))
            .unwrap_or_default();
        (&ident[..*name_len], email)
    }

    /// The authors whose name or email `matches` accepts, as a bitmap over
    /// author ids.
    pub fn select(&self, mut matches: impl FnMut(&str) -> bool) -> Vec<bool> {
        (0..self.authors.len() as u32)
            .map(|i| {
                let (name, email) = self.author(i);
                matches(name) || matches(email)
            })
            .collect()
    }

    /// The authors who changed `path_id`.
    pub fn authors_of(&self, path_id: u32) -> &[u32] {
        self.by_path
            .get(path_id as usize)
            .map_or(&[][..], Vec::as_slice)
    }

    /// Whether any author in `selected` (from [`select`](Self::select))
    /// changed `path_id`.
    pub fn any_selected(&self, path_id: u32, selected: &[bool]) -> bool {
        self.authors_of(path_id)
            .iter()
            .any(|&a| selected.get(a as usize).copied().unwrap_or(false))
    }
}

/// Loaded changes files of a repo node's repos, kept equal to their commit
/// logs.
#[derive(Clone)]
pub struct ChangeLogs {
    cache: moka::sync::Cache<RepoId, Arc<ChangeLog>>,
    locks: Arc<Mutex<HashMap<RepoId, Arc<Mutex<()>>>>>,
    git_bin: Option<PathBuf>,
}

impl std::fmt::Debug for ChangeLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeLogs")
            .field("entries", &self.cache.entry_count())
            .finish()
    }
}

impl ChangeLogs {
    /// A cache of up to `max_bytes` of loaded files.
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_, l: &Arc<ChangeLog>| {
                    u32::try_from(l.memory_bytes().max(1)).unwrap_or(u32::MAX)
                })
                .build(),
            locks: Arc::default(),
            git_bin: None,
        }
    }

    /// Lists large batches of commits with this `git` ([`update`]).
    pub fn with_git(mut self, git_bin: PathBuf) -> Self {
        self.git_bin = Some(git_bin);
        self
    }

    fn lock(&self, repo: RepoId) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if locks.len() > 1024 {
            locks.retain(|_, m| Arc::strong_count(m) > 1);
        }
        locks.entry(repo).or_default().clone()
    }

    fn cached(&self, repo: RepoId, path: &Path) -> Result<Option<Arc<ChangeLog>>> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.cache.invalidate(&repo);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if let Some(p) = self.cache.get(&repo)
            && p.dev_ino == (meta.dev(), meta.ino())
            && p.mtime == (meta.mtime(), meta.mtime_nsec())
            && p.file_len == meta.len()
        {
            return Ok(Some(p));
        }
        let log = ChangeLog::open(path)?.map(Arc::new);
        match &log {
            Some(l) => self.cache.insert(repo, l.clone()),
            None => self.cache.invalidate(&repo),
        }
        Ok(log)
    }

    /// The changes of `repo` (`repo` opened at `git_dir`) covering every
    /// commit of `log`, extending the file first when it lags behind.
    /// Blocking.
    pub fn current(
        &self,
        repo_id: RepoId,
        repo: &gix::Repository,
        log: &CommitLog,
    ) -> Result<Arc<ChangeLog>> {
        let path = changes_path(repo.git_dir());
        if let Ok(Some(c)) = self.cached(repo_id, &path)
            && c.len() == log.len()
            && c.aligned_with(log)
        {
            return Ok(c);
        }
        let lock = self.lock(repo_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.cached(repo_id, &path).ok().flatten();
        let started = std::time::Instant::now();
        let stats = update(repo, log, current.as_deref(), self.git_bin.as_deref())?;
        if stats.added > 0 {
            tracing::debug!(
                repo = repo_id.get(),
                added = stats.added,
                total = stats.total,
                rewritten = stats.rewritten,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "changes log updated"
            );
        }
        self.cached(repo_id, &path)?
            .ok_or_else(|| GitError::Corrupt(format!("{}: missing after update", path.display())))
    }

    /// Drops the cached file of `repo`.
    pub fn invalidate(&self, repo: RepoId) {
        self.cache.invalidate(&repo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str], author: &str, day: u32) -> String {
        let date = format!("2024-01-{day:02}T00:00:00Z");
        let (name, email) = author.split_once(' ').unwrap();
        let out = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", "Cid")
            .env("GIT_COMMITTER_EMAIL", "cid@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn commit(dir: &Path, files: &[(&str, &str)], author: &str, day: u32) {
        for (p, c) in files {
            let p = dir.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            if c.is_empty() {
                std::fs::remove_file(p).unwrap();
            } else {
                std::fs::write(p, c).unwrap();
            }
        }
        git(dir, &["add", "-A"], author, day);
        git(dir, &["commit", "-q", "-m", "c"], author, day);
    }

    fn open(dir: &Path) -> gix::Repository {
        gix::open(dir.join(".git")).unwrap()
    }

    fn snapshot(c: &ChangeLog) -> Vec<Vec<String>> {
        (0..c.len() as u32)
            .map(|p| c.changed_paths(p).map(str::to_owned).collect())
            .collect()
    }

    #[test]
    fn incremental_equals_full_build() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"], "A a@x", 1);
        commit(d, &[("a.txt", "1"), ("src/b.rs", "1")], "Ann ann@x", 1);
        commit(d, &[("src/b.rs", "2")], "Bob bob@x", 2);
        let repo = open(d);
        crate::commitlog::update(&repo, None).unwrap();
        let log_path = crate::commitlog::log_path(repo.git_dir());
        let log = CommitLog::open(&log_path).unwrap().unwrap();
        let s = update(&repo, &log, None, None).unwrap();
        assert_eq!((s.added, s.total, s.rewritten), (2, 2, true));

        // A branch and a merge, a deletion and a rename.
        git(d, &["checkout", "-q", "-b", "side"], "Ann ann@x", 3);
        commit(d, &[("docs/c.md", "1")], "Ann ann@x", 3);
        git(d, &["checkout", "-q", "main"], "Ann ann@x", 4);
        commit(d, &[("a.txt", "")], "Bob bob@x", 4);
        git(d, &["merge", "-q", "--no-edit", "side"], "Bob bob@x", 5);
        git(d, &["mv", "src/b.rs", "src/d.rs"], "Ann ann@x", 6);
        commit(d, &[], "Ann ann@x", 6);
        let repo = open(d);
        crate::commitlog::update(&repo, Some(&log)).unwrap();
        let log = CommitLog::open(&log_path).unwrap().unwrap();
        let s = update(&repo, &log, None, None).unwrap();
        assert_eq!((s.added, s.total, s.rewritten), (4, 6, false));
        let inc = ChangeLog::open(&changes_path(repo.git_dir()))
            .unwrap()
            .unwrap();

        std::fs::remove_file(changes_path(repo.git_dir())).unwrap();
        update(&repo, &log, None, None).unwrap();
        let full = ChangeLog::open(&changes_path(repo.git_dir()))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot(&inc), snapshot(&full));
        // `git log --raw` lists the same files.
        std::fs::remove_file(changes_path(repo.git_dir())).unwrap();
        let s = update_with(&repo, &log, None, Some(Path::new("git")), 0).unwrap();
        assert_eq!(s.added, 6);
        let by_git = ChangeLog::open(&changes_path(repo.git_dir()))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot(&by_git), snapshot(&full));
        let all: Vec<Vec<String>> = snapshot(&full);
        assert!(all.contains(&vec!["a.txt".to_owned(), "src/b.rs".to_owned()]));
        assert!(all.contains(&vec!["src/b.rs".to_owned(), "src/d.rs".to_owned()]));
        assert!(all.contains(&vec!["a.txt".to_owned()]));
        // The merge has no changes.
        assert!(all.contains(&Vec::new()));

        let idx = full.contributors(&log);
        assert_eq!(idx.author_count(), 2);
        let bob = idx.select(|s| s.contains("bob"));
        let ann = idx.select(|s| s == "Ann");
        let b = full.path_id("src/b.rs").unwrap();
        assert!(idx.any_selected(b, &bob) && idx.any_selected(b, &ann));
        let c = full.path_id("docs/c.md").unwrap();
        assert!(!idx.any_selected(c, &bob) && idx.any_selected(c, &ann));
    }

    #[test]
    fn misaligned_or_torn_files_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"], "A a@x", 1);
        commit(d, &[("a", "1")], "Ann ann@x", 1);
        commit(d, &[("b", "1")], "Ann ann@x", 2);
        let repo = open(d);
        crate::commitlog::update(&repo, None).unwrap();
        let log = CommitLog::open(&crate::commitlog::log_path(repo.git_dir()))
            .unwrap()
            .unwrap();
        update(&repo, &log, None, None).unwrap();
        let path = changes_path(repo.git_dir());
        let good = std::fs::read(&path).unwrap();
        // A torn tail is dropped and the missing record written again.
        std::fs::write(&path, &good[..good.len() - 3]).unwrap();
        let s = update(&repo, &log, None, None).unwrap();
        assert!(s.rewritten);
        assert_eq!(s.added, 1);
        assert_eq!(std::fs::read(&path).unwrap(), good);
        // Garbage is rebuilt.
        std::fs::write(&path, b"nonsense").unwrap();
        let logs = ChangeLogs::new(1 << 20);
        let c = logs.current(RepoId(1), &repo, &log).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), good);
    }
}
