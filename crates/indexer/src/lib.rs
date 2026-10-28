//! sourcegull-indexer: builds index shards from git trees.
//!
//! A full build walks each indexed branch's tree with `gix`, deduplicates
//! file versions across branches into one doc with a `branch_mask`, then
//! reads and prepares the blobs in parallel (in path order, so doc ids
//! follow paths) and streams them into a [`ShardWriter`] in batches; the
//! next batch is prepared while the writer fills the postings of the last.
//! Throughput: `scripts/bench/index-build.sh`. Files follow the
//! unindexed searcher's rules: over [`DEFAULT_MAX_FILE_SIZE`] (unless
//! [`ShardSpec::always_index`] matches the path) or binary (a NUL byte in the
//! first 8000 bytes) means no content, and the language
//! comes from the path, so `lang:` agrees across both backends.
//!
//! A delta build ([`plan_delta`], [`build_delta`]) compares the branch trees
//! with a base shard's docs path by path: each path whose versions or
//! branch masks differ is tombstoned in the base and written to the delta
//! with all its current versions.
//!
//! Small repositories are packed into compound shards ([`build_compound`])
//! by merging the shards that already index them.
//!
//! Symbols are extracted per file with `sourcegull-symbols` while the
//! blobs are prepared, so they cost no extra pass.
//!
//! Static ranks: a full build gives each doc its path's rank from
//! [`ShardSpec::ranks`] and records their generation; a delta keeps the
//! base's generation and gives each path the rank it has in the base (new
//! paths get 0 until the next full build).
//!
//! Limitations: deltas list the full tree rather than diffing trees, and
//! there is no external-memory merge for repositories larger than the
//! memory budget.

mod compound;

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use roaring::RoaringBitmap;
use sourcegull_gitstore::ObjectId;
use sourcegull_gitstore::parallel::{default_threads, map_ordered};
pub use sourcegull_gitstore::read::DEFAULT_MAX_FILE_SIZE;
use sourcegull_gitstore::read::{ListTreeOptions, is_binary, list_tree};
use sourcegull_index_format::{
    BranchMeta, Codec, Doc, DocInput, DocSymbol, FLAG_BINARY, FLAG_TOO_LARGE, PreparedDoc,
    RepoMeta, Shard, ShardWriter, prepare_doc,
};
use sourcegull_types::{AlwaysIndex, PathRanks};

pub use compound::{CompoundPart, build_compound};

/// At most this many branches per repository (`branch_mask` is a u64).
pub const MAX_BRANCHES: usize = 64;

/// Blobs read and prepared per parallel batch; bounds memory held at once.
const BATCH: usize = 2048;

/// Fewer files than this per thread are prepared on fewer threads.
const FILES_PER_THREAD: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error(transparent)]
    Git(#[from] sourcegull_gitstore::GitError),
    #[error(transparent)]
    Format(#[from] sourcegull_index_format::FormatError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = IndexerError> = std::result::Result<T, E>;

/// The repository a shard is built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSpec {
    pub repo_id: i32,
    pub name: String,
    pub rank: u16,
    /// Branch names and the commits to index, the default branch first.
    pub branches: Vec<(String, ObjectId)>,
    /// The files' static ranks for a full build; `None` leaves them unranked.
    pub ranks: Option<Arc<PathRanks>>,
    /// Files indexed with content above [`BuildOptions::max_file_size`].
    pub always_index: AlwaysIndex,
}

impl ShardSpec {
    /// The generation of [`ShardSpec::ranks`] as shards record it (0: none).
    pub fn ranks_generation(&self) -> u64 {
        self.ranks
            .as_ref()
            .map_or(0, |r| u64::try_from(r.generation).unwrap_or(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOptions {
    pub max_file_size: u64,
    pub codec: Codec,
    pub threads: usize,
    /// Extract symbols for the SYMBOLS section.
    pub symbols: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            codec: Codec::Lz4,
            threads: default_threads(),
            symbols: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Docs written (distinct file versions across branches).
    pub docs: usize,
    /// Text bytes indexed.
    pub content_bytes: u64,
    pub skipped_binary: usize,
    pub skipped_large: usize,
    /// Shard size in bytes.
    pub shard_bytes: u64,
    pub elapsed: Duration,
}

impl BuildStats {
    fn count(&mut self, docs: &[PreparedDoc]) {
        for doc in docs {
            self.docs += 1;
            self.content_bytes += doc.content_bytes();
            match doc.flags() {
                FLAG_BINARY => self.skipped_binary += 1,
                FLAG_TOO_LARGE => self.skipped_large += 1,
                _ => {}
            }
        }
    }
}

/// Symbols already extracted, from the shards a build replaces: looked up
/// by blob and language, so unchanged files are not parsed again.
#[derive(Default)]
pub struct KnownSymbols<'a> {
    docs: HashMap<([u8; 20], &'a str), (&'a Shard, Doc)>,
}

impl<'a> KnownSymbols<'a> {
    /// Adds the docs `docs` of `shard`, if its symbols come from the
    /// current extractor.
    pub fn add(&mut self, shard: &'a Shard, docs: std::ops::Range<u32>) {
        if shard.symbols_version() != sourcegull_symbols::VERSION {
            return;
        }
        for id in docs {
            let doc = shard.doc(id);
            if let Some(lang) = shard.lang(&doc).filter(|_| doc.has_content()) {
                self.docs.entry((doc.oid, lang)).or_insert((shard, doc));
            }
        }
    }

    fn get(&self, oid: &[u8; 20], lang: &str) -> Option<Vec<DocSymbol>> {
        let (shard, doc) = self.docs.get(&(*oid, lang))?;
        shard.symbols(doc).ok()
    }
}

/// Builds the shard for `spec` and writes it atomically to `out`.
pub fn build_shard(
    repo: &gix::Repository,
    spec: &ShardSpec,
    opts: &BuildOptions,
    known: &KnownSymbols<'_>,
    out: &Path,
) -> Result<BuildStats> {
    let start = Instant::now();
    let generation = spec.ranks_generation();
    let (writer, mut stats) = collect(
        repo,
        spec,
        opts,
        known,
        full_entries(repo, spec)?,
        generation,
    )?;
    stats.shard_bytes = writer.finish(out)?;
    stats.elapsed = start.elapsed();
    Ok(stats)
}

/// The changes from a base shard to new branch heads.
#[derive(Debug)]
pub struct DeltaPlan {
    entries: Vec<Entry>,
    base: u32,
    /// The base's ranks generation for the repository, which the delta keeps.
    ranks_generation: u64,
    /// Base docs the delta replaces.
    pub tombstones: RoaringBitmap,
    /// The repository's docs in the base.
    pub base_docs: u32,
}

impl DeltaPlan {
    /// Docs the delta will hold.
    pub fn docs(&self) -> usize {
        self.entries.len()
    }
}

/// Compares `spec`'s branch trees with `spec`'s repository in `base` (a
/// single-repo or compound shard). `None` when a delta cannot express the
/// change: `base` is a delta itself, does not hold the repository, or holds
/// different branch names or a different order.
pub fn plan_delta(
    repo: &gix::Repository,
    spec: &ShardSpec,
    base: &Shard,
) -> Result<Option<DeltaPlan>> {
    let Some(idx) = base.repos().iter().position(|r| r.id == spec.repo_id) else {
        return Ok(None);
    };
    let meta = &base.repos()[idx];
    let same_branches = meta.branches.len() == spec.branches.len()
        && meta
            .branches
            .iter()
            .zip(&spec.branches)
            .all(|(m, (name, _))| &m.name == name);
    if !same_branches || base.tombstones().is_some() {
        return Ok(None);
    }
    // Base versions in (path, oid) order, as the writer stored them.
    let range = base.repo_docs(idx as u16);
    let mut old: Vec<(String, [u8; 20], u64, u32)> = Vec::with_capacity(range.len());
    base.walk_docs(range.start, &mut |d, p| {
        if d.id >= range.end {
            return ControlFlow::Break(());
        }
        old.push((p.to_owned(), d.oid, d.branch_mask, d.id));
        ControlFlow::Continue(())
    })?;
    let new = entries(repo, spec)?;
    let mut plan = DeltaPlan {
        entries: Vec::new(),
        base: base.id(),
        ranks_generation: base.ranks_generation(idx as u16),
        tombstones: RoaringBitmap::new(),
        base_docs: range.len() as u32,
    };
    let mut i = 0;
    let mut new_iter = new.into_iter().peekable();
    let mut group: Vec<Entry> = Vec::new();
    while i < old.len() || new_iter.peek().is_some() {
        let path = match (old.get(i), new_iter.peek()) {
            (Some(o), Some(n)) => o.0.as_str().min(n.path.as_str()).to_owned(),
            (Some(o), None) => o.0.clone(),
            (None, Some(n)) => n.path.clone(),
            (None, None) => break,
        };
        let mut j = i;
        while old.get(j).is_some_and(|o| o.0 == path) {
            j += 1;
        }
        group.clear();
        while new_iter.peek().is_some_and(|n| n.path == path) {
            group.extend(new_iter.next());
        }
        let same = j - i == group.len()
            && old[i..j]
                .iter()
                .zip(&group)
                .all(|(o, n)| o.1 == n.oid.as_bytes() && o.2 == n.mask);
        if !same {
            // The path keeps its rank in the base.
            let rank = old[i..j].first().map_or(0.0, |o| base.doc_rank(o.3));
            for e in &mut group {
                e.rank = rank;
            }
            plan.tombstones.extend(old[i..j].iter().map(|o| o.3));
            plan.entries.append(&mut group);
        }
        i = j;
    }
    Ok(Some(plan))
}

/// Writes the delta shard planned by [`plan_delta`] atomically to `out`.
pub fn build_delta(
    repo: &gix::Repository,
    spec: &ShardSpec,
    plan: DeltaPlan,
    opts: &BuildOptions,
    known: &KnownSymbols<'_>,
    out: &Path,
) -> Result<BuildStats> {
    let start = Instant::now();
    let (mut writer, mut stats) =
        collect(repo, spec, opts, known, plan.entries, plan.ranks_generation)?;
    writer.set_tombstones(plan.base, plan.tombstones);
    stats.shard_bytes = writer.finish(out)?;
    stats.elapsed = start.elapsed();
    Ok(stats)
}

/// [`build_shard`] into memory (ephemeral indexes, tests).
pub fn build_shard_to_vec(
    repo: &gix::Repository,
    spec: &ShardSpec,
    opts: &BuildOptions,
) -> Result<(Vec<u8>, BuildStats)> {
    let start = Instant::now();
    let known = KnownSymbols::default();
    let generation = spec.ranks_generation();
    let (writer, mut stats) = collect(
        repo,
        spec,
        opts,
        &known,
        full_entries(repo, spec)?,
        generation,
    )?;
    let bytes = writer.finish_to_vec()?;
    stats.shard_bytes = bytes.len() as u64;
    stats.elapsed = start.elapsed();
    Ok((bytes, stats))
}

/// A file version and the branches holding it.
#[derive(Debug)]
struct Entry {
    path: String,
    oid: ObjectId,
    mask: u64,
    rank: f32,
}

/// [`entries`] with their ranks from `spec.ranks`, for a full build.
fn full_entries(repo: &gix::Repository, spec: &ShardSpec) -> Result<Vec<Entry>> {
    let mut out = entries(repo, spec)?;
    if let Some(ranks) = &spec.ranks {
        for e in &mut out {
            e.rank = ranks.get(&e.path);
        }
    }
    Ok(out)
}

/// Lists every blob of every branch, merged by (path, oid).
fn entries(repo: &gix::Repository, spec: &ShardSpec) -> Result<Vec<Entry>> {
    let mut by_version: HashMap<(String, ObjectId), u64> = HashMap::new();
    let opts = ListTreeOptions {
        recursive: true,
        limit: usize::MAX,
        ..ListTreeOptions::default()
    };
    for (bit, (_, commit)) in spec.branches.iter().enumerate() {
        list_tree(repo, *commit, &opts, |e| {
            if e.kind.is_blob() {
                *by_version.entry((e.path, e.oid)).or_default() |= 1 << bit;
            }
            ControlFlow::Continue(())
        })?;
    }
    let mut out: Vec<Entry> = by_version
        .into_iter()
        .map(|((path, oid), mask)| Entry {
            path,
            oid,
            mask,
            rank: 0.0,
        })
        .collect();
    out.sort_unstable_by(|a, b| (&a.path, a.oid).cmp(&(&b.path, b.oid)));
    Ok(out)
}

fn prepare(
    repo: &gix::Repository,
    e: &Entry,
    opts: &BuildOptions,
    always_index: &AlwaysIndex,
    known: &KnownSymbols<'_>,
) -> sourcegull_gitstore::Result<PreparedDoc> {
    let size = repo.find_header(e.oid)?.size();
    let mut input = DocInput {
        repo: 0,
        branch_mask: e.mask,
        path: &e.path,
        oid: e.oid.as_bytes().try_into().expect("SHA-1 ids"),
        lang: sourcegull_languages::detect(&e.path, None).map(|l| l.id),
        size,
        flags: 0,
        content: &[],
        symbols: &[],
    };
    if size > always_index.max_file_size(&e.path, opts.max_file_size) {
        input.flags = FLAG_TOO_LARGE;
        return Ok(prepare_doc(&input, opts.codec).with_rank(e.rank));
    }
    let blob = repo.find_blob(e.oid)?;
    let mut symbols = Vec::new();
    if is_binary(&blob.data) {
        input.flags = FLAG_BINARY;
    } else {
        input.content = &blob.data;
        if let Some(lang) = input.lang.filter(|_| opts.symbols) {
            symbols = known
                .get(&input.oid, lang)
                .unwrap_or_else(|| doc_symbols(lang, &blob.data));
        }
    }
    Ok(prepare_doc(&input, opts.codec)
        .with_symbols(symbols)
        .with_rank(e.rank))
}

/// The symbols defined in a file, for the shard.
pub fn doc_symbols(lang: &str, content: &[u8]) -> Vec<DocSymbol> {
    sourcegull_symbols::extract(lang, content)
        .into_iter()
        .map(|s| DocSymbol {
            name: s.name,
            container: s.container,
            kind: s.kind as u8,
            range: s.range,
        })
        .collect()
}

fn collect(
    repo: &gix::Repository,
    spec: &ShardSpec,
    opts: &BuildOptions,
    known: &KnownSymbols<'_>,
    mut rest: Vec<Entry>,
    ranks_generation: u64,
) -> Result<(ShardWriter, BuildStats)> {
    if spec.branches.is_empty() || spec.branches.len() > MAX_BRANCHES {
        return Err(IndexerError::Invalid(format!(
            "{} branches (1 to {MAX_BRANCHES})",
            spec.branches.len()
        )));
    }
    let mut writer = ShardWriter::new(opts.codec).with_threads(opts.threads);
    if opts.symbols {
        writer.set_symbols_version(sourcegull_symbols::VERSION);
    }
    let idx = writer.add_repo(RepoMeta {
        id: spec.repo_id,
        name: spec.name.clone(),
        rank: spec.rank,
        branches: spec
            .branches
            .iter()
            .map(|(name, commit)| BranchMeta {
                name: name.clone(),
                commit: commit.as_bytes().try_into().expect("SHA-1 ids"),
            })
            .collect(),
    })?;
    writer.set_ranks_generation(idx, ranks_generation)?;
    let mut stats = BuildStats::default();
    let shared = repo.clone().into_sync();
    let mut batches = Vec::with_capacity(rest.len().div_ceil(BATCH));
    while !rest.is_empty() {
        let tail = rest.split_off(rest.len().min(BATCH));
        batches.push(std::mem::replace(&mut rest, tail));
    }
    let prepare_batch = |batch: Vec<Entry>| {
        // A thread per FILES_PER_THREAD files at most: each costs a spawn,
        // a repository handle and its parsers, more than a few small files.
        let threads = opts.threads.min(batch.len().div_ceil(FILES_PER_THREAD));
        map_ordered(&shared, batch, threads, |r, e| {
            prepare(r, &e, opts, &spec.always_index, known)
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
    };
    if opts.threads == 1 || batches.len() < 2 {
        for batch in batches {
            let docs = prepare_batch(batch)?;
            stats.count(&docs);
            writer.push_batch(docs)?;
        }
        return Ok((writer, stats));
    }
    // The next batch is prepared while the writer adds this one's postings.
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        s.spawn(move || {
            for batch in batches {
                let docs = prepare_batch(batch);
                let failed = docs.is_err();
                if tx.send(docs).is_err() || failed {
                    break;
                }
            }
        });
        for docs in rx {
            let docs = docs?;
            stats.count(&docs);
            writer.push_batch(docs)?;
        }
        Ok::<_, IndexerError>(())
    })?;
    Ok((writer, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_index_format::Shard;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "init.defaultBranch=main"])
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

    fn oid(dir: &Path, rev: &str) -> ObjectId {
        ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim().as_bytes()).unwrap()
    }

    #[test]
    fn builds_and_dedups_branches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/main.go"), "package main\n").unwrap();
        std::fs::write(p.join("README.md"), "# readme\n").unwrap();
        std::fs::write(p.join("blob.bin"), b"a\0b").unwrap();
        std::fs::write(p.join("big.txt"), "x".repeat(100)).unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "one"]);
        git(p, &["checkout", "-qb", "dev"]);
        std::fs::write(p.join("src/main.go"), "package main\n\nfunc Dev() {}\n").unwrap();
        std::fs::write(p.join("dev.txt"), "dev only\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "two"]);
        let repo = gix::open(p).unwrap();
        let spec = ShardSpec {
            repo_id: 5,
            name: "g/r".into(),
            rank: 0,
            branches: vec![
                ("main".into(), oid(p, "main")),
                ("dev".into(), oid(p, "dev")),
            ],
            ranks: None,
            always_index: Default::default(),
        };
        let opts = BuildOptions {
            max_file_size: 64,
            threads: 2,
            ..BuildOptions::default()
        };
        let (bytes, stats) = build_shard_to_vec(&repo, &spec, &opts).unwrap();
        assert_eq!(stats.docs, 6);
        assert_eq!((stats.skipped_binary, stats.skipped_large), (1, 1));
        assert_eq!(stats.shard_bytes, bytes.len() as u64);
        let shard = Shard::from_vec(bytes).unwrap();
        assert_eq!(shard.repos()[0].branches[1].name, "dev");
        let docs: Vec<(String, u64, Option<String>)> = shard
            .docs()
            .map(|d| {
                (
                    shard.path(&d).unwrap(),
                    d.branch_mask,
                    shard.lang(&d).map(str::to_owned),
                )
            })
            .collect();
        let paths: Vec<_> = docs.iter().map(|(p, m, _)| (p.as_str(), *m)).collect();
        // Unchanged files are one doc on both branches; src/main.go has two.
        assert_eq!(
            paths[..4],
            [
                ("README.md", 3),
                ("big.txt", 3),
                ("blob.bin", 3),
                ("dev.txt", 2)
            ]
        );
        assert_eq!(paths[4].0, "src/main.go");
        assert_eq!(paths[5].0, "src/main.go");
        assert_eq!(paths[4].1 | paths[5].1, 3);
        assert_eq!(docs[4].2.as_deref(), Some("go"));
        let big = shard.doc(1);
        assert!(!big.has_content());
        assert_eq!(big.size, 100);
        let main_dev = shard
            .docs()
            .find(|d| d.branch_mask == 2 && d.path == shard.doc(4).path)
            .unwrap();
        assert_eq!(
            &*shard.content(&main_dev).unwrap(),
            b"package main\n\nfunc Dev() {}\n"
        );
        let names: Vec<_> = shard
            .symbols(&main_dev)
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.kind, s.range))
            .collect();
        assert_eq!(
            names,
            [("main".into(), 4, 8..12), ("Dev".into(), 12, 19..22)]
        );
        assert!(!shard.has_symbols(&shard.doc(0)));

        let out = dir.path().join("5.gullshard");
        let stats = build_shard(&repo, &spec, &opts, &KnownSymbols::default(), &out).unwrap();
        assert_eq!(std::fs::metadata(&out).unwrap().len(), stats.shard_bytes);

        // A rebuild takes symbols from a shard holding the same blob, when
        // they come from the current extractor.
        let fake = |version| {
            let mut w = ShardWriter::new(Codec::Lz4);
            w.set_symbols_version(version);
            let r = w.add_repo(shard.repos()[0].clone()).unwrap();
            let fake = [DocSymbol {
                name: "Fake".into(),
                container: None,
                kind: 12,
                range: 0..4,
            }];
            let content = shard.content(&main_dev).unwrap();
            w.push(prepare_doc(
                &DocInput {
                    repo: r,
                    branch_mask: 1,
                    path: "x.go",
                    oid: main_dev.oid,
                    lang: Some("go"),
                    size: content.len() as u64,
                    flags: 0,
                    content: &content,
                    symbols: &fake,
                },
                Codec::Lz4,
            ))
            .unwrap();
            Shard::from_vec(w.finish_to_vec().unwrap()).unwrap()
        };
        for (version, want) in [(sourcegull_symbols::VERSION, "Fake"), (0, "main")] {
            let old = fake(version);
            let mut known = KnownSymbols::default();
            known.add(&old, 0..1);
            build_shard(&repo, &spec, &opts, &known, &out).unwrap();
            let rebuilt = Shard::open(&out).unwrap();
            let doc = rebuilt.docs().find(|d| d.oid == main_dev.oid).unwrap();
            assert_eq!(rebuilt.symbols(&doc).unwrap()[0].name, want);
            assert_eq!(rebuilt.symbols_version(), sourcegull_symbols::VERSION);
        }

        let none = ShardSpec {
            branches: vec![],
            ..spec
        };
        assert!(build_shard_to_vec(&repo, &none, &opts).is_err());
    }

    #[test]
    fn always_index_reads_large_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::create_dir_all(p.join("gen")).unwrap();
        std::fs::write(p.join("gen/api.ts"), "y".repeat(100)).unwrap();
        std::fs::write(p.join("big.txt"), "x".repeat(100)).unwrap();
        std::fs::write(
            p.join("gen/blob.bin"),
            [b'a'; 100].iter().chain(b"\0").copied().collect::<Vec<_>>(),
        )
        .unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "one"]);
        let repo = gix::open(p).unwrap();
        let spec = ShardSpec {
            repo_id: 1,
            name: "g/r".into(),
            rank: 0,
            branches: vec![("main".into(), oid(p, "HEAD"))],
            ranks: None,
            always_index: AlwaysIndex::new(["gen/*"]).unwrap(),
        };
        let opts = BuildOptions {
            max_file_size: 64,
            ..BuildOptions::default()
        };
        let (bytes, stats) = build_shard_to_vec(&repo, &spec, &opts).unwrap();
        assert_eq!((stats.skipped_binary, stats.skipped_large), (1, 1));
        let shard = Shard::from_vec(bytes).unwrap();
        let flags: Vec<(String, u16)> = shard
            .docs()
            .map(|d| (shard.path(&d).unwrap(), d.flags))
            .collect();
        assert_eq!(
            flags,
            [
                ("big.txt".to_owned(), FLAG_TOO_LARGE),
                ("gen/api.ts".to_owned(), 0),
                ("gen/blob.bin".to_owned(), FLAG_BINARY),
            ]
        );
    }

    #[test]
    fn parallel_build_equals_serial() {
        // More files than one batch, so the threaded build prepares a batch
        // while the writer fills the previous one's postings in parallel.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        for i in 0..BATCH + 300 {
            let d = p.join(format!("d{}", i % 17));
            std::fs::create_dir_all(&d).unwrap();
            let body: String = (0..20)
                .map(|j| format!("fn f{i}_{j}(x: u{}) -> W{} {{}}\n", i * 31 + j, i ^ j))
                .collect();
            std::fs::write(d.join(format!("f{i}.rs")), body).unwrap();
        }
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "many"]);
        let repo = gix::open(p).unwrap();
        let spec = ShardSpec {
            repo_id: 1,
            name: "g/r".into(),
            rank: 0,
            branches: vec![("main".into(), oid(p, "main"))],
            ranks: None,
            always_index: Default::default(),
        };
        let build = |threads| {
            let opts = BuildOptions {
                threads,
                symbols: false,
                ..BuildOptions::default()
            };
            build_shard_to_vec(&repo, &spec, &opts).unwrap()
        };
        let (serial, stats) = build(1);
        assert_eq!(stats.docs, BATCH + 300);
        let (parallel, _) = build(4);
        assert!(serial == parallel, "shards differ");
        let shard = Shard::from_vec(serial).unwrap();
        assert_eq!(shard.doc_count() as usize, BATCH + 300);
    }

    #[test]
    fn ranks_from_spec_and_base() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::write(p.join("a.go"), "package a\n").unwrap();
        std::fs::write(p.join("b.go"), "package b\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "one"]);
        let first = oid(p, "HEAD");
        let repo = gix::open(p).unwrap();
        let ranks = PathRanks::new(4, [("a.go".to_owned(), 0.5), ("b.go".to_owned(), 1.0)]);
        let spec = ShardSpec {
            repo_id: 1,
            name: "g/r".into(),
            rank: 0,
            branches: vec![("HEAD".into(), first)],
            ranks: Some(Arc::new(ranks)),
            always_index: Default::default(),
        };
        let opts = BuildOptions::default();
        let (bytes, _) = build_shard_to_vec(&repo, &spec, &opts).unwrap();
        let base = Shard::from_vec(bytes).unwrap();
        assert_eq!(base.ranks_generation(0), 4);
        let half = sourcegull_types::ranking::dequantize(sourcegull_types::ranking::quantize(0.5));
        assert_eq!((base.doc_rank(0), base.doc_rank(1)), (half, 1.0));

        // A delta keeps the base's generation and ranks; new paths get 0.
        std::fs::write(p.join("b.go"), "package b // changed\n").unwrap();
        std::fs::write(p.join("c.go"), "package c\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "two"]);
        let repo = gix::open(p).unwrap();
        let next = ShardSpec {
            branches: vec![("HEAD".into(), oid(p, "HEAD"))],
            ranks: None,
            always_index: Default::default(),
            ..spec.clone()
        };
        let plan = plan_delta(&repo, &next, &base).unwrap().unwrap();
        let out = p.join("delta.gullshard");
        build_delta(&repo, &next, plan, &opts, &KnownSymbols::default(), &out).unwrap();
        let delta = Shard::open(&out).unwrap();
        assert_eq!(delta.ranks_generation(0), 4);
        let got: Vec<(String, f32)> = delta
            .docs()
            .map(|d| (delta.path(&d).unwrap(), delta.doc_rank(d.id)))
            .collect();
        assert_eq!(got, [("b.go".into(), 1.0), ("c.go".into(), 0.0)]);

        // Without ranks, a full build has none.
        let (bytes, _) = build_shard_to_vec(&repo, &next, &opts).unwrap();
        let plain = Shard::from_vec(bytes).unwrap();
        assert_eq!(plain.ranks_generation(0), 0);
        assert!((0..plain.doc_count()).all(|i| plain.doc_rank(i) == 0.0));
    }
}
