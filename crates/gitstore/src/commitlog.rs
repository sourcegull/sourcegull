//! Per-repo commit metadata log.
//!
//! `<repo>.git/sourcegull/commits.log` holds one record per commit reachable from
//! any ref: oid, tree, parents (as positions in the log), author, committer
//! and message. Parents are written before their children, so after a fetch
//! the log is extended by appending the new commits ([`update`]), and commit
//! search walks history over the log ([`CommitLog::walk`]) without reading
//! git objects. Commits that became unreachable stay in the log; a walk from
//! a commit only reaches its ancestors.
//!
//! The file is an 8-byte magic and an 8-byte nonce (new for every file, so a
//! replaced log is never taken for an extended one), followed by records: `u32 len`, `u32 crc32`
//! of the payload, then the payload (little-endian):
//!
//! ```text
//! oid[20] tree[20] author_time:i64 author_tz:i32 committer_time:i64 committer_tz:i32
//! nparents:u32 parent:u32 × nparents        (u32::MAX: parent not in the log)
//! author_name_len:u32 author_len:u32 author "name <email>"
//! committer_name_len:u32 committer_len:u32 committer "name <email>"
//! message (the rest)
//! ```
//!
//! Bytes once written never change: the writer only appends, and replaces
//! the file by a rename when it has to drop a torn record left by a crash.
//! Readers map the file and map it again to see appended records.

use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::ObjectId;
use sourcegull_types::RepoId;

use crate::error::{GitError, Result};
use crate::read::{self, CommitInfo, Signature};
use crate::refs;

const MAGIC: &[u8; 8] = b"SKCLOG01";
const HEADER: usize = 16;
const OID: usize = 20;
/// Offset of `nparents` in a payload.
const FIXED: usize = 2 * OID + 2 * (8 + 4);
const NO_PARENT: u32 = u32::MAX;

/// The log file of the bare repo at `git_dir`.
pub fn log_path(git_dir: &Path) -> PathBuf {
    git_dir.join("sourcegull").join("commits.log")
}

enum Data {
    Mmap(memmap2::Mmap),
    Empty,
}

impl Data {
    fn bytes(&self) -> &[u8] {
        match self {
            Data::Mmap(m) => m,
            Data::Empty => &[],
        }
    }
}

/// A loaded commit log: the mapped file and an index over its records.
pub struct CommitLog {
    data: Data,
    /// Start of each record's payload.
    offsets: Vec<u64>,
    /// Record positions sorted by oid.
    by_oid: Vec<u32>,
    /// Bytes covered by valid records (the header included).
    valid_len: u64,
    /// File size when mapped.
    file_len: u64,
    /// File identity and mtime, to notice a changed or replaced file.
    dev_ino: (u64, u64),
    mtime: (i64, i64),
    nonce: u64,
}

impl std::fmt::Debug for CommitLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitLog")
            .field("commits", &self.offsets.len())
            .field("valid_len", &self.valid_len)
            .field("file_len", &self.file_len)
            .finish_non_exhaustive()
    }
}

impl CommitLog {
    /// Maps and indexes the log at `path`. `Ok(None)` when there is none.
    pub fn open(path: &Path) -> Result<Option<CommitLog>> {
        Self::open_from(path, None)
    }

    /// Like [`open`](Self::open), reusing the index of `prev` (an earlier
    /// load of the same file) so only appended records are parsed.
    pub fn open_from(path: &Path, prev: Option<&CommitLog>) -> Result<Option<CommitLog>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        let dev_ino = (meta.dev(), meta.ino());
        let mtime = (meta.mtime(), meta.mtime_nsec());
        let file_len = meta.len();
        let data = if file_len == 0 {
            Data::Empty
        } else {
            // SAFETY: the log is append-only and replaced only by rename
            // (module docs), so the mapped bytes never change. Every record
            // is bounds-checked when indexed.
            #[allow(unsafe_code)]
            let map = unsafe { memmap2::Mmap::map(&file)? };
            Data::Mmap(map)
        };
        let bytes = data.bytes();
        if bytes.len() < HEADER || &bytes[..MAGIC.len()] != MAGIC {
            return Err(GitError::Corrupt(format!(
                "{}: not a commit log",
                path.display()
            )));
        }
        let nonce = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let prev = prev.filter(|p| p.nonce == nonce && p.file_len <= file_len);
        let (mut offsets, by_oid, mut at) = match prev {
            Some(p) => (p.offsets.clone(), p.by_oid.clone(), p.valid_len as usize),
            None => (Vec::new(), Vec::new(), HEADER),
        };
        let first_new = offsets.len();
        while let Some((payload, next)) = record_at(bytes, at, offsets.len()) {
            offsets.push(payload as u64);
            at = next;
        }
        let mut log = CommitLog {
            data,
            offsets,
            by_oid,
            valid_len: at as u64,
            file_len,
            dev_ino,
            mtime,
            nonce,
        };
        log.index_from(first_new);
        Ok(Some(log))
    }

    /// Adds records `first..` to the oid index.
    fn index_from(&mut self, first: usize) {
        let end = self.offsets.len();
        if first == end {
            return;
        }
        // Sort by an 8-byte prefix first: oids are uniform, so full
        // comparisons are needed only for the rare equal prefixes.
        let prefix =
            |i: u32| u64::from_be_bytes(self.oid_bytes(i)[..8].try_into().expect("8 bytes"));
        let mut keyed: Vec<(u64, u32)> =
            (first as u32..end as u32).map(|i| (prefix(i), i)).collect();
        keyed.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| self.oid_bytes(a.1).cmp(self.oid_bytes(b.1)))
        });
        let new: Vec<u32> = keyed.into_iter().map(|(_, i)| i).collect();
        if first == 0 {
            self.by_oid = new;
            return;
        }
        let old = std::mem::take(&mut self.by_oid);
        let mut merged = Vec::with_capacity(old.len() + new.len());
        let (mut i, mut j) = (0, 0);
        while i < old.len() && j < new.len() {
            if self.oid_bytes(old[i]) <= self.oid_bytes(new[j]) {
                merged.push(old[i]);
                i += 1;
            } else {
                merged.push(new[j]);
                j += 1;
            }
        }
        merged.extend_from_slice(&old[i..]);
        merged.extend_from_slice(&new[j..]);
        self.by_oid = merged;
    }

    /// Number of commits.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Whether the log has no commits.
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Whether the file ends in a partial record (a crash or a write in
    /// progress).
    pub fn has_torn_tail(&self) -> bool {
        self.valid_len < self.file_len
    }

    /// Approximate memory held by the index.
    pub fn index_bytes(&self) -> usize {
        self.offsets.len() * 8 + self.by_oid.len() * 4
    }

    fn oid_bytes(&self, i: u32) -> &[u8] {
        let at = self.offsets[i as usize] as usize;
        &self.data.bytes()[at..at + OID]
    }

    /// The position of `oid` in the log.
    pub fn lookup(&self, oid: &ObjectId) -> Option<u32> {
        let key = oid.as_bytes();
        self.by_oid
            .binary_search_by(|&i| self.oid_bytes(i).cmp(key))
            .ok()
            .map(|k| self.by_oid[k])
    }

    /// The commit at position `i` (`i < len()`).
    pub fn get(&self, i: u32) -> LogCommit<'_> {
        let at = self.offsets[i as usize] as usize;
        let len = u32_at(self.data.bytes(), at - 8) as usize;
        LogCommit {
            log: self,
            pos: i,
            p: &self.data.bytes()[at..at + len],
        }
    }

    /// History from position `start`, newest first by committer time (ties:
    /// later in the log first), each commit once.
    pub fn walk(&self, start: u32) -> Walk<'_> {
        let mut seen = vec![0u64; self.len().div_ceil(64)];
        seen[start as usize / 64] |= 1 << (start % 64);
        let mut heap = BinaryHeap::new();
        heap.push((self.get(start).committer_time(), start));
        Walk {
            log: self,
            heap,
            seen,
        }
    }
}

/// Parses the record header at `at` and checks the payload. Returns the
/// payload's start and the next record's start.
fn record_at(bytes: &[u8], at: usize, pos: usize) -> Option<(usize, usize)> {
    let header = bytes.get(at..at + 8)?;
    let len = u32_at(header, 0) as usize;
    let crc = u32_at(header, 4);
    let payload = bytes.get(at + 8..(at + 8).checked_add(len)?)?;
    (crc32fast::hash(payload) == crc && payload_ok(payload, pos)).then_some((at + 8, at + 8 + len))
}

/// Whether `p` is well-formed for record `pos`: fields fit and parents come
/// earlier in the log.
fn payload_ok(p: &[u8], pos: usize) -> bool {
    let Some(n) = p.get(FIXED..FIXED + 4).map(|b| u32_at(b, 0) as usize) else {
        return false;
    };
    let mut at = FIXED + 4;
    for _ in 0..n {
        let Some(b) = p.get(at..at + 4) else {
            return false;
        };
        let parent = u32_at(b, 0);
        if parent != NO_PARENT && parent as usize >= pos {
            return false;
        }
        at += 4;
    }
    for _ in 0..2 {
        let Some(b) = p.get(at..at + 8) else {
            return false;
        };
        let (name_len, len) = (u32_at(b, 0) as usize, u32_at(b, 4) as usize);
        at += 8;
        match p.get(at..at + len) {
            Some(s) if name_len <= len && std::str::from_utf8(s).is_ok() => at += len,
            _ => return false,
        }
    }
    std::str::from_utf8(&p[at..]).is_ok()
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// A person on a commit, borrowed from the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogIdent<'a> {
    /// `name <email>`, as commit filters match it.
    pub ident: &'a str,
    name_len: usize,
    /// Seconds since the epoch.
    pub time: i64,
    /// Offset from UTC in seconds.
    pub tz_offset_seconds: i32,
}

impl<'a> LogIdent<'a> {
    /// Name.
    pub fn name(&self) -> &'a str {
        &self.ident[..self.name_len]
    }

    /// Email.
    pub fn email(&self) -> &'a str {
        self.ident
            .get(self.name_len + 2..self.ident.len() - 1)
            .unwrap_or_default()
    }

    fn to_signature(self) -> Signature {
        Signature {
            name: self.name().to_owned(),
            email: self.email().to_owned(),
            time: self.time,
            tz_offset_seconds: self.tz_offset_seconds,
        }
    }
}

/// A commit in the log.
#[derive(Clone, Copy)]
pub struct LogCommit<'a> {
    log: &'a CommitLog,
    pos: u32,
    p: &'a [u8],
}

impl std::fmt::Debug for LogCommit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogCommit")
            .field("pos", &self.pos)
            .field("oid", &self.oid())
            .finish_non_exhaustive()
    }
}

impl<'a> LogCommit<'a> {
    /// Position in the log.
    pub fn pos(&self) -> u32 {
        self.pos
    }

    /// Commit id.
    pub fn oid(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.p[..OID])
    }

    /// Root tree id.
    pub fn tree(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.p[OID..2 * OID])
    }

    /// Committer time, seconds since the epoch.
    pub fn committer_time(&self) -> i64 {
        i64_at(self.p, 2 * OID + 12)
    }

    fn parent_count(&self) -> usize {
        u32_at(self.p, FIXED) as usize
    }

    /// Number of parents.
    pub fn is_merge(&self) -> bool {
        self.parent_count() > 1
    }

    /// Parent positions in order; `None` for a parent missing from the log.
    pub fn parents(&self) -> impl Iterator<Item = Option<u32>> + 'a {
        let p = self.p;
        (0..self.parent_count()).map(move |k| {
            let v = u32_at(p, FIXED + 4 + 4 * k);
            (v != NO_PARENT).then_some(v)
        })
    }

    fn idents_start(&self) -> usize {
        FIXED + 4 + 4 * self.parent_count()
    }

    fn ident_at(&self, at: usize, time_at: usize) -> (LogIdent<'a>, usize) {
        let name_len = u32_at(self.p, at) as usize;
        let len = u32_at(self.p, at + 4) as usize;
        let s = &self.p[at + 8..at + 8 + len];
        let ident = LogIdent {
            ident: std::str::from_utf8(s).unwrap_or_default(),
            name_len,
            time: i64_at(self.p, time_at),
            tz_offset_seconds: i32_at(self.p, time_at + 8),
        };
        (ident, at + 8 + len)
    }

    /// Author.
    pub fn author(&self) -> LogIdent<'a> {
        self.ident_at(self.idents_start(), 2 * OID).0
    }

    /// Committer.
    pub fn committer(&self) -> LogIdent<'a> {
        let (_, next) = self.ident_at(self.idents_start(), 2 * OID);
        self.ident_at(next, 2 * OID + 12).0
    }

    /// Full message.
    pub fn message(&self) -> &'a str {
        let (_, next) = self.ident_at(self.idents_start(), 2 * OID);
        let (_, next) = self.ident_at(next, 2 * OID + 12);
        std::str::from_utf8(&self.p[next..]).unwrap_or_default()
    }

    /// The commit as [`read::commit_info`] returns it. A parent missing from
    /// the log is left out.
    pub fn to_info(&self) -> CommitInfo {
        CommitInfo {
            oid: self.oid(),
            parents: self
                .parents()
                .flatten()
                .map(|i| self.log.get(i).oid())
                .collect(),
            author: self.author().to_signature(),
            committer: self.committer().to_signature(),
            message: self.message().to_owned(),
            tree: self.tree(),
        }
    }
}

/// See [`CommitLog::walk`].
pub struct Walk<'a> {
    log: &'a CommitLog,
    heap: BinaryHeap<(i64, u32)>,
    seen: Vec<u64>,
}

impl<'a> Iterator for Walk<'a> {
    type Item = LogCommit<'a>;

    fn next(&mut self) -> Option<LogCommit<'a>> {
        let (_, i) = self.heap.pop()?;
        let c = self.log.get(i);
        for p in c.parents().flatten() {
            let (w, b) = (p as usize / 64, 1u64 << (p % 64));
            if self.seen[w] & b == 0 {
                self.seen[w] |= b;
                self.heap.push((self.log.get(p).committer_time(), p));
            }
        }
        Some(c)
    }
}

/// What [`update`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Commits appended.
    pub added: usize,
    /// Commits in the log afterwards.
    pub total: usize,
    /// Whether the file was written anew (first build, or a torn or
    /// unreadable log replaced).
    pub rewritten: bool,
}

/// Extends the log of `repo` with the commits reachable from its refs and
/// HEAD that it lacks. `current` is a load of the log to start from (it is
/// loaded when `None`). One writer per repo at a time (the caller holds the
/// repo's lock).
pub fn update(repo: &gix::Repository, current: Option<&CommitLog>) -> Result<UpdateStats> {
    let path = log_path(repo.git_dir());
    let loaded;
    let current = match current {
        Some(c) => Some(c),
        None => {
            loaded = match CommitLog::open(&path) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "rebuilding the commit log");
                    None
                }
            };
            loaded.as_ref()
        }
    };
    let base = current.map_or(0, CommitLog::len);
    let known = |oid: &ObjectId, new: &HashMap<ObjectId, u32>| {
        new.get(oid)
            .copied()
            .or_else(|| current.and_then(|l| l.lookup(oid)))
    };

    let mut tips: Vec<ObjectId> = refs::snapshot(repo)?.into_values().collect();
    if let Ok(head) = repo.head_id() {
        tips.push(head.detach());
    }
    let mut out = Vec::new();
    let mut new: HashMap<ObjectId, u32> = HashMap::new();
    // Depth-first, parents before children: (commit, parents, next parent).
    let mut stack: Vec<(ObjectId, Vec<ObjectId>, usize)> = Vec::new();
    for tip in tips {
        let Some(tip) = peel_to_commit(repo, tip) else {
            continue;
        };
        if known(&tip, &new).is_some() {
            continue;
        }
        let Some(parents) = parent_ids(repo, tip) else {
            continue;
        };
        stack.push((tip, parents, 0));
        while let Some((oid, parents, next)) = stack.last_mut() {
            if let Some(&p) = parents.get(*next) {
                *next += 1;
                if known(&p, &new).is_none()
                    && let Some(pp) = parent_ids(repo, p)
                {
                    stack.push((p, pp, 0));
                }
                continue;
            }
            let oid = *oid;
            stack.pop();
            let info = read::commit_info(repo, oid)?;
            let pos = (base + new.len()) as u32;
            let parents: Vec<u32> = info
                .parents
                .iter()
                .map(|p| known(p, &new).unwrap_or(NO_PARENT))
                .collect();
            encode(&mut out, &info, &parents);
            new.insert(oid, pos);
        }
    }

    let rewrite = current.is_none_or(CommitLog::has_torn_tail);
    if new.is_empty() && !rewrite {
        return Ok(UpdateStats {
            added: 0,
            total: base,
            rewritten: false,
        });
    }
    if rewrite {
        let dir = path.parent().expect("log path has a parent");
        std::fs::create_dir_all(dir)?;
        let tmp = tempfile::NamedTempFile::new_in(dir)?;
        let mut w = BufWriter::new(tmp.as_file());
        w.write_all(MAGIC)?;
        w.write_all(&new_nonce().to_le_bytes())?;
        if let Some(c) = current {
            w.write_all(&c.data.bytes()[HEADER..c.valid_len as usize])?;
        }
        w.write_all(&out)?;
        w.flush()?;
        drop(w);
        tmp.as_file().sync_data()?;
        tmp.persist(&path).map_err(|e| GitError::Io(e.error))?;
    } else {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path)?;
        f.write_all(&out)?;
        f.sync_data()?;
    }
    Ok(UpdateStats {
        added: new.len(),
        total: base + new.len(),
        rewritten: rewrite,
    })
}

fn new_nonce() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u64) ^ (u64::from(std::process::id()) << 40)
}

fn peel_to_commit(repo: &gix::Repository, id: ObjectId) -> Option<ObjectId> {
    let obj = repo.find_object(id).ok()?.peel_tags_to_end().ok()?;
    (obj.kind == gix::object::Kind::Commit).then_some(obj.id)
}

/// The parents of `id`, or `None` if it is not a readable commit.
fn parent_ids(repo: &gix::Repository, id: ObjectId) -> Option<Vec<ObjectId>> {
    let c = repo.find_commit(id).ok()?;
    Some(c.parent_ids().map(|p| p.detach()).collect())
}

fn encode(out: &mut Vec<u8>, c: &CommitInfo, parents: &[u32]) {
    let start = out.len();
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(c.oid.as_bytes());
    out.extend_from_slice(c.tree.as_bytes());
    for s in [&c.author, &c.committer] {
        out.extend_from_slice(&s.time.to_le_bytes());
        out.extend_from_slice(&s.tz_offset_seconds.to_le_bytes());
    }
    out.extend_from_slice(&(parents.len() as u32).to_le_bytes());
    for p in parents {
        out.extend_from_slice(&p.to_le_bytes());
    }
    for s in [&c.author, &c.committer] {
        let ident = format!("{} <{}>", s.name, s.email);
        out.extend_from_slice(&(s.name.len() as u32).to_le_bytes());
        out.extend_from_slice(&(ident.len() as u32).to_le_bytes());
        out.extend_from_slice(ident.as_bytes());
    }
    out.extend_from_slice(c.message.as_bytes());
    let payload = &out[start + 8..];
    let len = (payload.len() as u32).to_le_bytes();
    let crc = crc32fast::hash(payload).to_le_bytes();
    out[start..start + 4].copy_from_slice(&len);
    out[start + 4..start + 8].copy_from_slice(&crc);
}

/// Loaded logs of a repo node's repos (cheap to clone, shared), revalidated against the file on each
/// access (a stat) and extended by parsing only appended records.
#[derive(Clone)]
pub struct CommitLogs {
    cache: moka::sync::Cache<RepoId, Arc<CommitLog>>,
}

impl std::fmt::Debug for CommitLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitLogs")
            .field("entries", &self.cache.entry_count())
            .finish()
    }
}

impl CommitLogs {
    /// A cache holding indexes of up to `max_index_bytes` in total.
    pub fn new(max_index_bytes: u64) -> Self {
        Self {
            cache: moka::sync::Cache::builder()
                .max_capacity(max_index_bytes)
                .weigher(|_, l: &Arc<CommitLog>| {
                    u32::try_from(l.index_bytes().max(1)).unwrap_or(u32::MAX)
                })
                .build(),
        }
    }

    /// The current log of `repo` (bare repo at `git_dir`), or `None` when it
    /// has none yet.
    pub fn get(&self, repo: RepoId, git_dir: &Path) -> Result<Option<Arc<CommitLog>>> {
        let path = log_path(git_dir);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.cache.invalidate(&repo);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let prev = self.cache.get(&repo);
        if let Some(p) = &prev
            && p.dev_ino == (meta.dev(), meta.ino())
            && p.mtime == (meta.mtime(), meta.mtime_nsec())
            && p.file_len == meta.len()
        {
            return Ok(prev);
        }
        let log = CommitLog::open_from(&path, prev.as_deref())?.map(Arc::new);
        match &log {
            Some(l) => self.cache.insert(repo, l.clone()),
            None => self.cache.invalidate(&repo),
        }
        Ok(log)
    }

    /// Drops the cached log of `repo`.
    pub fn invalidate(&self, repo: RepoId) {
        self.cache.invalidate(&repo);
    }
}

/// [`update`] for the repo at `git_dir`, starting from `current`, logging
/// instead of failing: a missing log only makes commit search fall back to
/// walking git.
pub fn update_logged(git_dir: &Path, current: Option<&CommitLog>) {
    let started = std::time::Instant::now();
    let r = crate::sync::open(git_dir).and_then(|r| update(&r.to_thread_local(), current));
    match r {
        Ok(s) => tracing::debug!(
            repo = %git_dir.display(),
            added = s.added,
            total = s.total,
            rewritten = s.rewritten,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "commit log updated"
        ),
        Err(e) => {
            tracing::warn!(repo = %git_dir.display(), error = %e, "commit log update failed");
        }
    }
}

/// Whether `s` (`name <email>`) is valid as written by [`encode`]; used by tests.
#[cfg(test)]
fn split_ident(s: &str) -> Option<(&str, &str)> {
    let (name, rest) = s.split_once(" <")?;
    Some((name, rest.strip_suffix('>')?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str], date: &str) -> String {
        let out = Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "Ann Author")
            .env("GIT_AUTHOR_EMAIL", "ann@example.com")
            .env("GIT_COMMITTER_NAME", "Cid")
            .env("GIT_COMMITTER_EMAIL", "cid@example.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
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

    fn commit(dir: &Path, msg: &str, day: u32) -> ObjectId {
        let date = format!("2024-01-{day:02}T00:00:00Z");
        git(dir, &["commit", "-q", "--allow-empty", "-m", msg], &date);
        ObjectId::from_hex(git(dir, &["rev-parse", "HEAD"], &date).as_bytes()).unwrap()
    }

    fn walk_oids(log: &CommitLog, start: ObjectId) -> Vec<ObjectId> {
        log.walk(log.lookup(&start).unwrap())
            .map(|c| c.oid())
            .collect()
    }

    fn git_log(repo: &gix::Repository, start: ObjectId) -> Vec<CommitInfo> {
        let opts = read::LogOptions {
            limit: usize::MAX,
            ..Default::default()
        };
        read::log(repo, start, &opts).unwrap().commits
    }

    #[test]
    fn builds_extends_and_walks_like_git() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"], "2024-01-01T00:00:00Z");
        commit(p, "one", 1);
        commit(p, "two\n\nwith a body ✓", 2);
        git(p, &["checkout", "-q", "-b", "side"], "2024-01-03T00:00:00Z");
        commit(p, "side", 4);
        git(p, &["checkout", "-q", "main"], "2024-01-03T00:00:00Z");
        commit(p, "three", 3);
        git(
            p,
            &["merge", "-q", "--no-ff", "-m", "merge", "side"],
            "2024-01-05T00:00:00Z",
        );
        let repo = gix::open(p).unwrap();
        let s = update(&repo, None).unwrap();
        assert_eq!((s.added, s.total, s.rewritten), (5, 5, true));
        let path = log_path(repo.git_dir());
        let log = CommitLog::open(&path).unwrap().unwrap();
        let head = repo.head_id().unwrap().detach();
        let expected = git_log(&repo, head);
        let walked: Vec<CommitInfo> = log
            .walk(log.lookup(&head).unwrap())
            .map(|c| c.to_info())
            .collect();
        assert_eq!(walked, expected);
        let c = log.get(log.lookup(&head).unwrap());
        assert!(c.is_merge());
        assert_eq!(c.author().name(), "Ann Author");
        assert_eq!(c.author().email(), "ann@example.com");
        assert_eq!(
            split_ident(c.committer().ident),
            Some(("Cid", "cid@example.com"))
        );

        // Nothing new: no write.
        let s = update(&repo, Some(&log)).unwrap();
        assert_eq!((s.added, s.rewritten), (0, false));

        // New commits are appended and picked up incrementally.
        let four = commit(p, "four", 6);
        let s = update(&repo, Some(&log)).unwrap();
        assert_eq!((s.added, s.total, s.rewritten), (1, 6, false));
        let log2 = CommitLog::open_from(&path, Some(&log)).unwrap().unwrap();
        assert_eq!(log2.len(), 6);
        assert_eq!(
            walk_oids(&log2, four),
            git_log(&repo, four)
                .iter()
                .map(|c| c.oid)
                .collect::<Vec<_>>()
        );
        let fresh = CommitLog::open(&path).unwrap().unwrap();
        assert_eq!(fresh.by_oid, log2.by_oid);
    }

    #[test]
    fn torn_tail_is_ignored_then_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"], "2024-01-01T00:00:00Z");
        commit(p, "one", 1);
        let repo = gix::open(p).unwrap();
        update(&repo, None).unwrap();
        let path = log_path(repo.git_dir());
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&[42, 0, 0, 0, 1, 2]).unwrap();
        drop(f);
        let log = CommitLog::open(&path).unwrap().unwrap();
        assert_eq!(log.len(), 1);
        assert!(log.has_torn_tail());
        let two = commit(p, "two", 2);
        let s = update(&repo, Some(&log)).unwrap();
        assert_eq!((s.added, s.total, s.rewritten), (1, 2, true));
        let log = CommitLog::open(&path).unwrap().unwrap();
        assert!(!log.has_torn_tail());
        assert_eq!(walk_oids(&log, two).len(), 2);
    }

    #[test]
    fn cache_follows_appends_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"], "2024-01-01T00:00:00Z");
        commit(p, "one", 1);
        let repo = gix::open(p).unwrap();
        let logs = CommitLogs::new(1 << 20);
        let id = RepoId(1);
        assert!(logs.get(id, repo.git_dir()).unwrap().is_none());
        update(&repo, None).unwrap();
        let a = logs.get(id, repo.git_dir()).unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &a,
            &logs.get(id, repo.git_dir()).unwrap().unwrap()
        ));
        commit(p, "two", 2);
        update(&repo, Some(&a)).unwrap();
        let b = logs.get(id, repo.git_dir()).unwrap().unwrap();
        assert_eq!(b.len(), 2);
        std::fs::remove_file(log_path(repo.git_dir())).unwrap();
        update(&repo, None).unwrap();
        let c = logs.get(id, repo.git_dir()).unwrap().unwrap();
        assert_eq!(c.len(), 2);
        assert_ne!(c.nonce, b.nonce);
    }
}
