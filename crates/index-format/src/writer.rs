//! Building shards.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use roaring::RoaringBitmap;
use rustc_hash::FxHashMap;

use crate::paths::PathsBuilder;
use crate::symbols::SymbolsBuilder;
use crate::{
    DOC_ROW, Doc, DocSymbol, FLAG_BINARY, FLAG_TOO_LARGE, FormatError, HEADER_FIXED, MAGIC,
    RepoMeta, Result, SECTIONS, Section, Shard, TOC_ENTRY, VERSION, postings, trigram, varint,
};

/// How doc content is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Codec {
    None,
    #[default]
    Lz4,
}

impl Codec {
    pub(crate) fn flag(self) -> u32 {
        match self {
            Codec::None => 0,
            Codec::Lz4 => 1,
        }
    }

    pub(crate) fn from_flag(f: u32) -> Option<Codec> {
        match f & 0xff {
            0 => Some(Codec::None),
            1 => Some(Codec::Lz4),
            _ => None,
        }
    }
}

/// One file version to index.
#[derive(Debug, Clone, Copy)]
pub struct DocInput<'a> {
    /// Index of the repository in the shard ([`ShardWriter::add_repo`]).
    pub repo: u16,
    pub branch_mask: u64,
    pub path: &'a str,
    pub oid: [u8; 20],
    /// A language id (`sourcegull_languages::Language::id`).
    pub lang: Option<&'a str>,
    /// Full file size in bytes.
    pub size: u64,
    /// [`FLAG_BINARY`] or [`FLAG_TOO_LARGE`] mark files without content.
    pub flags: u16,
    /// The text content; ignored for binary and too-large files.
    pub content: &'a [u8],
    /// Symbols defined in the content; ignored without content.
    pub symbols: &'a [DocSymbol],
}

/// A doc with its content compressed, newlines and trigrams extracted:
/// the per-file work of a build, done off the writer's thread.
#[derive(Debug, Clone)]
pub struct PreparedDoc {
    repo: u16,
    branch_mask: u64,
    path: String,
    oid: [u8; 20],
    lang: Option<String>,
    size: u32,
    flags: u16,
    stored: Vec<u8>,
    newlines: Vec<u8>,
    line_count: u32,
    /// Distinct, in no particular order.
    trigrams: Vec<u32>,
    symbols: Vec<DocSymbol>,
    rank: f32,
}

impl PreparedDoc {
    /// Sets the doc's static rank, clamped to [0, 1].
    pub fn with_rank(mut self, rank: f32) -> Self {
        self.rank = if rank.is_finite() {
            rank.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self
    }

    pub fn content_bytes(&self) -> u64 {
        if self.has_content() {
            u64::from(self.size)
        } else {
            0
        }
    }

    /// Sets the doc's symbols, as [`DocInput::symbols`] does without a
    /// copy; ignored without content.
    pub fn with_symbols(mut self, symbols: Vec<DocSymbol>) -> Self {
        if self.has_content() {
            self.symbols = symbols;
        }
        self
    }

    /// [`FLAG_BINARY`], [`FLAG_TOO_LARGE`] or 0.
    pub fn flags(&self) -> u16 {
        self.flags
    }

    fn has_content(&self) -> bool {
        self.flags & (FLAG_BINARY | FLAG_TOO_LARGE) == 0
    }
}

pub fn prepare_doc(input: &DocInput<'_>, codec: Codec) -> PreparedDoc {
    let mut doc = PreparedDoc {
        repo: input.repo,
        branch_mask: input.branch_mask,
        path: input.path.to_owned(),
        oid: input.oid,
        lang: input.lang.map(str::to_owned),
        size: input.size.min(u64::from(u32::MAX)) as u32,
        flags: input.flags,
        stored: Vec::new(),
        newlines: Vec::new(),
        line_count: 0,
        trigrams: Vec::new(),
        symbols: Vec::new(),
        rank: 0.0,
    };
    if !doc.has_content() {
        return doc;
    }
    doc.symbols = input.symbols.to_vec();
    let content = input.content;
    doc.size = content.len().min(u32::MAX as usize) as u32;
    doc.stored = match codec {
        Codec::None => content.to_vec(),
        Codec::Lz4 => lz4_flex::block::compress(content),
    };
    let mut prev = 0u64;
    for (i, nl) in memchr::memchr_iter(b'\n', content).enumerate() {
        let nl = nl as u64;
        varint::put(&mut doc.newlines, if i == 0 { nl } else { nl - prev });
        prev = nl;
        doc.line_count += 1;
    }
    if content.last().is_some_and(|&b| b != b'\n') {
        doc.line_count += 1;
    }
    doc.trigrams = trigram::distinct(content);
    doc
}

/// Prepares a copy of `doc` from `shard` as a doc of repository `repo` in
/// a new shard written with `codec` (compound shards). Stored
/// content and newlines are copied as they are when the codecs agree; the
/// trigrams are extracted again from the content. The doc keeps its rank.
pub fn prepare_copy(shard: &Shard, doc: &Doc, repo: u16, codec: Codec) -> Result<PreparedDoc> {
    let content = shard.content(doc)?;
    let path = shard.path(doc)?;
    let symbols = shard.symbols(doc)?;
    let input = DocInput {
        repo,
        branch_mask: doc.branch_mask,
        path: &path,
        oid: doc.oid,
        lang: shard.lang(doc),
        size: u64::from(doc.size),
        flags: doc.flags,
        content: &content,
        symbols: &symbols,
    };
    let rank = shard.doc_rank(doc.id);
    if codec != shard.codec() || !doc.has_content() {
        return Ok(prepare_doc(&input, codec).with_rank(rank));
    }
    let raw = shard.raw_doc(doc);
    let trigrams = trigram::distinct(&content);
    Ok(PreparedDoc {
        repo,
        branch_mask: doc.branch_mask,
        path,
        oid: doc.oid,
        lang: shard.lang(doc).map(str::to_owned),
        size: doc.size,
        flags: doc.flags,
        stored: raw.0.to_vec(),
        newlines: raw.1.to_vec(),
        line_count: doc.line_count,
        trigrams,
        symbols,
        rank,
    })
}

/// Collects docs (pushed in (repo, path) order) and writes a shard.
pub struct ShardWriter {
    codec: Codec,
    repos: Vec<RepoMeta>,
    langs: Vec<String>,
    lang_ids: HashMap<String, u16>,
    docs: Vec<u8>,
    doc_count: u32,
    paths: PathsBuilder,
    last_repo: Option<u16>,
    content: Vec<u8>,
    newlines: Vec<u8>,
    /// Doc ids per trigram, partitioned by the trigram's first byte so the
    /// partitions in order are in key order and can be filled and encoded
    /// in parallel.
    postings: Vec<PostingsPart>,
    threads: usize,
    tombstones: Option<(u32, RoaringBitmap)>,
    symbols: SymbolsBuilder,
    /// Per doc, its static rank.
    ranks: Vec<f32>,
    /// Per repository, the generation of its ranks.
    rank_generations: Vec<u64>,
}

/// The doc lists of the trigrams starting with one byte, found by the
/// trigram's low 16 bits: in a small shard through a hash map, then
/// through a direct table (no hashing), so in a large one they come out in
/// key order without sorting. A table per part costs 256 KiB to clear and
/// 65,536 slots to scan, more than a small shard's whole build.
#[derive(Default)]
struct PostingsPart {
    /// Per low 16 bits, 1 + the index in `lists` (0: no docs yet); empty
    /// until the part holds [`DENSE`] lists.
    slots: Vec<u32>,
    /// Until then, the same by low 16 bits.
    sparse: FxHashMap<u16, u32>,
    /// The low 16 bits and docs of each trigram.
    lists: Vec<(u16, Vec<u32>)>,
}

/// Parts with more trigrams use a direct table.
const DENSE: usize = 2048;

impl PostingsPart {
    /// Appends `docs` (ascending, above any added before) to the trigrams'
    /// lists: (low 16 bits of the trigram, doc id) pairs.
    fn add(&mut self, docs: &[(u16, u32)]) {
        if self.slots.is_empty() && self.lists.len() + docs.len() > DENSE {
            self.slots = vec![0; 1 << 16];
            for (low, i) in self.sparse.drain() {
                self.slots[usize::from(low)] = i;
            }
        }
        if self.slots.is_empty() {
            for &(low, id) in docs {
                let next = self.lists.len() as u32 + 1;
                let slot = *self.sparse.entry(low).or_insert(next);
                if slot == next {
                    self.lists.push((low, Vec::new()));
                }
                self.lists[slot as usize - 1].1.push(id);
            }
            return;
        }
        for &(low, id) in docs {
            let slot = &mut self.slots[usize::from(low)];
            if *slot == 0 {
                self.lists.push((low, Vec::new()));
                *slot = self.lists.len() as u32;
            }
            self.lists[*slot as usize - 1].1.push(id);
        }
    }
}

const PARTS: usize = 256;

/// Shards this large or larger are synced to disk when written
/// ([`ShardWriter::finish`]).
pub const SYNC_MIN: u64 = 16 << 20;

/// Below this many trigrams in a batch, postings are filled on one thread.
const PARALLEL_MIN: usize = 1 << 16;

impl ShardWriter {
    pub fn new(codec: Codec) -> Self {
        ShardWriter {
            codec,
            repos: Vec::new(),
            langs: Vec::new(),
            lang_ids: HashMap::new(),
            docs: Vec::new(),
            doc_count: 0,
            paths: PathsBuilder::default(),
            last_repo: None,
            content: Vec::new(),
            newlines: Vec::new(),
            postings: (0..PARTS).map(|_| PostingsPart::default()).collect(),
            threads: 1,
            tombstones: None,
            symbols: SymbolsBuilder::default(),
            ranks: Vec::new(),
            rank_generations: Vec::new(),
        }
    }

    /// Fills and encodes postings on up to `n` threads.
    pub fn with_threads(mut self, n: usize) -> Self {
        self.threads = n.max(1);
        self
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Records the version of the symbol extractor that produced the docs'
    /// symbols; 0 (the default) means none were extracted.
    pub fn set_symbols_version(&mut self, version: u32) {
        self.symbols.version = version;
    }

    /// Makes this a delta shard over the base shard with id `base` (see
    /// [`Shard::id`](crate::Shard::id)), hiding the base docs in `docs`.
    pub fn set_tombstones(&mut self, base: u32, docs: RoaringBitmap) {
        self.tombstones = Some((base, docs));
    }

    /// Records the generation of the ranks given to repository `repo`'s
    /// docs ([`PreparedDoc::with_rank`]); 0 (the default) means none.
    pub fn set_ranks_generation(&mut self, repo: u16, generation: u64) -> Result<()> {
        let slot = self
            .rank_generations
            .get_mut(usize::from(repo))
            .ok_or_else(|| FormatError::Invalid(format!("unknown repo {repo}")))?;
        *slot = generation;
        Ok(())
    }

    /// Adds a repository; docs refer to it by the returned index.
    pub fn add_repo(&mut self, repo: RepoMeta) -> Result<u16> {
        if repo.branches.len() > 64 {
            return Err(FormatError::Invalid(format!(
                "{} branches (at most 64)",
                repo.branches.len()
            )));
        }
        let idx = u16::try_from(self.repos.len())
            .map_err(|_| FormatError::Invalid("too many repositories".into()))?;
        self.repos.push(repo);
        self.rank_generations.push(0);
        Ok(idx)
    }

    /// Appends a doc and returns its id. Docs must come in (repo, path)
    /// order; versions of one path are adjacent and share its path entry.
    /// After an error the writer must be discarded.
    pub fn push(&mut self, doc: PreparedDoc) -> Result<u32> {
        let ids = self.push_batch(vec![doc])?;
        Ok(ids.start)
    }

    /// [`ShardWriter::push`] for consecutive docs; their postings are filled
    /// in parallel. Returns the ids given.
    pub fn push_batch(&mut self, docs: Vec<PreparedDoc>) -> Result<std::ops::Range<u32>> {
        let first = self.doc_count;
        let mut trigrams = Vec::with_capacity(docs.len());
        for mut doc in docs {
            let ts = std::mem::take(&mut doc.trigrams);
            trigrams.push((self.append(doc)?, ts));
        }
        fill_postings(&mut self.postings, &trigrams, self.threads);
        Ok(first..self.doc_count)
    }

    fn append(&mut self, mut doc: PreparedDoc) -> Result<u32> {
        let repo = self
            .repos
            .get(usize::from(doc.repo))
            .ok_or_else(|| FormatError::Invalid(format!("unknown repo {}", doc.repo)))?;
        let branches = repo.branches.len();
        if branches < 64 && doc.branch_mask >> branches != 0 {
            return Err(FormatError::Invalid(format!(
                "{}: branch mask {:#x} names unknown branches",
                doc.path, doc.branch_mask
            )));
        }
        let same_repo = self.last_repo == Some(doc.repo);
        if self.last_repo.is_some_and(|r| r > doc.repo)
            || (same_repo && self.paths.last().is_some_and(|p| p > doc.path.as_bytes()))
        {
            return Err(FormatError::Invalid(format!(
                "{}: docs must be pushed in (repo, path) order",
                doc.path
            )));
        }
        if !(same_repo && self.paths.last() == Some(doc.path.as_bytes())) {
            self.paths.push(doc.path.as_bytes());
        }
        self.last_repo = Some(doc.repo);
        let path = self.paths.len() - 1;
        let id = self.doc_count;
        let lang = match doc.lang {
            None => 0,
            Some(l) => match self.lang_ids.get(&l) {
                Some(&i) => i,
                None => {
                    self.langs.push(l.clone());
                    let i = self.langs.len() as u16;
                    self.lang_ids.insert(l, i);
                    i
                }
            },
        };
        let row = &mut self.docs;
        row.extend_from_slice(&doc.repo.to_le_bytes());
        row.extend_from_slice(&lang.to_le_bytes());
        row.extend_from_slice(&doc.flags.to_le_bytes());
        row.extend_from_slice(&0u16.to_le_bytes());
        row.extend_from_slice(&doc.branch_mask.to_le_bytes());
        row.extend_from_slice(&path.to_le_bytes());
        row.extend_from_slice(&doc.size.to_le_bytes());
        row.extend_from_slice(&doc.line_count.to_le_bytes());
        row.extend_from_slice(&(doc.stored.len() as u32).to_le_bytes());
        row.extend_from_slice(&(self.content.len() as u64).to_le_bytes());
        row.extend_from_slice(&(self.newlines.len() as u64).to_le_bytes());
        row.extend_from_slice(&(doc.newlines.len() as u32).to_le_bytes());
        row.extend_from_slice(&doc.oid);
        debug_assert_eq!(row.len(), (id as usize + 1) * DOC_ROW);
        if !self.symbols.fits() {
            return Err(FormatError::Invalid("too many symbols".into()));
        }
        self.symbols
            .push_doc(std::mem::take(&mut doc.symbols), doc.size);
        self.ranks.push(doc.rank);
        self.content.extend_from_slice(&doc.stored);
        self.newlines.extend_from_slice(&doc.newlines);
        self.doc_count = id
            .checked_add(1)
            .ok_or_else(|| FormatError::Invalid("too many docs".into()))?;
        Ok(id)
    }

    /// Serializes the shard.
    pub fn finish_to_vec(self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.write(&mut out)?;
        Ok(out)
    }

    /// Writes the shard to `path` atomically: a temporary file next to it is
    /// written and renamed over `path`. Shards of [`SYNC_MIN`] bytes or more
    /// are synced, the file before the rename and the directory after it;
    /// smaller ones are not. Returns the size in bytes.
    ///
    /// A shard is derived data. One that a crash leaves half written fails
    /// its checksums, which [`Shard::open`](crate::Shard::open) checks, and
    /// the index deletes it; a lost rename leaves the shard it replaced,
    /// whose branches no longer match. The index's startup pass rebuilds
    /// both. Two fsyncs cost 10–12 ms on WSL2 and on many cloud volumes,
    /// several times the build of a small repository, while a large
    /// shard's rebuild costs more than its sync.
    pub fn finish(self, path: &Path) -> Result<u64> {
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        let mut w = std::io::BufWriter::with_capacity(1 << 20, tmp.as_file_mut());
        let n = self.write(&mut w)?;
        w.flush()?;
        drop(w);
        let sync = n >= SYNC_MIN;
        if sync {
            tmp.as_file().sync_all()?;
        }
        tmp.persist(path).map_err(|e| e.error)?;
        if sync && let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(n)
    }

    fn write(self, w: &mut impl Write) -> Result<u64> {
        let parts = encode_postings(self.postings, self.threads);
        let mut postings_data = Vec::with_capacity(parts.iter().map(|p| p.1.len()).sum());
        let mut fst = fst::MapBuilder::memory();
        for (keys, data) in parts {
            let base = postings_data.len() as u64;
            for (t, offset, count) in keys {
                fst.insert(trigram::key(t), postings::fst_value(base + offset, count))
                    .expect("keys inserted in order");
            }
            postings_data.extend_from_slice(&data);
        }
        let fst = fst.into_inner().expect("in-memory FST");

        let mut repos = Vec::new();
        put_u32(&mut repos, self.repos.len() as u32);
        for r in &self.repos {
            repos.extend_from_slice(&r.id.to_le_bytes());
            repos.extend_from_slice(&r.rank.to_le_bytes());
            repos.extend_from_slice(&(r.branches.len() as u16).to_le_bytes());
            put_str(&mut repos, &r.name);
            for b in &r.branches {
                put_str(&mut repos, &b.name);
                repos.extend_from_slice(&b.commit);
            }
        }
        let mut langs = Vec::new();
        put_u32(&mut langs, self.langs.len() as u32);
        for l in &self.langs {
            put_str(&mut langs, l);
        }

        let mut bodies: Vec<(Section, Vec<u8>)> = vec![
            (Section::Repos, repos),
            (Section::Langs, langs),
            (Section::Docs, self.docs),
            (Section::Paths, self.paths.finish()),
            (Section::Content, self.content),
            (Section::Newlines, self.newlines),
            (Section::Trigrams, fst),
            (Section::Postings, postings_data),
            (Section::Symbols, self.symbols.finish()),
        ];
        debug_assert_eq!(bodies.len(), SECTIONS.len());
        if let Some((base, docs)) = &self.tombstones {
            let mut body = Vec::with_capacity(4 + docs.serialized_size());
            put_u32(&mut body, *base);
            docs.serialize_into(&mut body)?;
            bodies.push((Section::Tombstones, body));
        }
        if self.rank_generations.iter().any(|&g| g != 0) || self.ranks.iter().any(|&r| r != 0.0) {
            let mut body =
                Vec::with_capacity(4 + 8 * self.rank_generations.len() + 4 * self.ranks.len());
            put_u32(&mut body, self.rank_generations.len() as u32);
            for g in &self.rank_generations {
                body.extend_from_slice(&g.to_le_bytes());
            }
            for r in &self.ranks {
                body.extend_from_slice(&r.to_le_bytes());
            }
            bodies.push((Section::Ranks, body));
        }
        let header_len = HEADER_FIXED + TOC_ENTRY * bodies.len() + 4;
        let mut header = Vec::with_capacity(header_len);
        header.extend_from_slice(MAGIC);
        put_u32(&mut header, VERSION);
        put_u32(&mut header, self.codec.flag());
        put_u32(&mut header, bodies.len() as u32);
        put_u32(&mut header, 0);
        let mut offset = header_len as u64;
        for (kind, body) in &bodies {
            put_u32(&mut header, *kind as u32);
            put_u32(&mut header, 0);
            header.extend_from_slice(&offset.to_le_bytes());
            header.extend_from_slice(&(body.len() as u64).to_le_bytes());
            put_u32(&mut header, crc32fast::hash(body));
            put_u32(&mut header, 0);
            offset += body.len() as u64;
        }
        let crc = crc32fast::hash(&header);
        put_u32(&mut header, crc);
        w.write_all(&header)?;
        for (_, body) in &bodies {
            w.write_all(body)?;
        }
        Ok(offset)
    }
}

/// Appends doc ids to their trigrams' lists. Thread `k` owns the
/// partitions `p` with `p % threads == k`; each first gathers its pairs per
/// partition, so filling one partition touches only its own table.
fn fill_postings(parts: &mut [PostingsPart], docs: &[(u32, Vec<u32>)], threads: usize) {
    let total: usize = docs.iter().map(|d| d.1.len()).sum();
    let threads = if total < PARALLEL_MIN { 1 } else { threads };
    let fill = |k: usize, group: Vec<&mut PostingsPart>| {
        let mut pending: Vec<Vec<(u16, u32)>> = (0..group.len()).map(|_| Vec::new()).collect();
        for (id, ts) in docs {
            for &t in ts {
                let p = (t >> 16) as usize;
                if p % threads == k {
                    pending[p / threads].push((t as u16, *id));
                }
            }
        }
        for (part, pairs) in group.into_iter().zip(pending) {
            if !pairs.is_empty() {
                part.add(&pairs);
            }
        }
    };
    let mut groups: Vec<Vec<&mut PostingsPart>> = (0..threads).map(|_| Vec::new()).collect();
    for (p, part) in parts.iter_mut().enumerate() {
        groups[p % threads].push(part);
    }
    if threads == 1 {
        fill(0, groups.pop().expect("one group"));
        return;
    }
    std::thread::scope(|s| {
        for (k, group) in groups.into_iter().enumerate() {
            let fill = &fill;
            s.spawn(move || fill(k, group));
        }
    });
}

/// Per partition, in order: (trigram, offset in the partition's data, doc
/// count) in key order, and the encoded lists.
type EncodedPart = (Vec<(u32, u64, usize)>, Vec<u8>);

fn encode_part(p: usize, mut part: PostingsPart) -> EncodedPart {
    let mut keys = Vec::with_capacity(part.lists.len());
    let mut data = Vec::new();
    let mut put = |low: u16, docs: &[u32]| {
        keys.push((
            (p << 16) as u32 | u32::from(low),
            data.len() as u64,
            docs.len(),
        ));
        postings::encode(&mut data, docs);
    };
    if part.slots.is_empty() {
        part.lists.sort_unstable_by_key(|l| l.0);
        for (low, docs) in &part.lists {
            put(*low, docs);
        }
    } else {
        for &slot in &part.slots {
            if slot != 0 {
                let (low, docs) = &part.lists[slot as usize - 1];
                put(*low, docs);
            }
        }
    }
    (keys, data)
}

fn encode_postings(parts: Vec<PostingsPart>, threads: usize) -> Vec<EncodedPart> {
    // A small shard's lists encode faster than threads start.
    let lists: usize = parts.iter().map(|p| p.lists.len()).sum();
    if threads == 1 || lists < PARALLEL_MIN {
        return parts
            .into_iter()
            .enumerate()
            .map(|(p, part)| encode_part(p, part))
            .collect();
    }
    let mut groups: Vec<Vec<(usize, PostingsPart)>> = (0..threads).map(|_| Vec::new()).collect();
    for (p, part) in parts.into_iter().enumerate() {
        groups[p % threads].push((p, part));
    }
    let mut out: Vec<(usize, EncodedPart)> = std::thread::scope(|s| {
        let handles: Vec<_> = groups
            .into_iter()
            .map(|g| {
                s.spawn(move || {
                    g.into_iter()
                        .map(|(p, part)| (p, encode_part(p, part)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("postings encoder panicked"))
            .collect()
    });
    out.sort_unstable_by_key(|(p, _)| *p);
    out.into_iter().map(|(_, e)| e).collect()
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_and_dense_parts_encode_alike() {
        // 3,000 trigrams over ascending docs, in pseudo-random order.
        let pairs: Vec<(u16, u32)> = (0..9000u32)
            .map(|i| (((i % 3000) * 7919 % 65_521) as u16, i / 3000))
            .collect();
        let mut reference: Vec<(u16, Vec<u32>)> = Vec::new();
        for &(low, id) in &pairs {
            match reference.iter_mut().find(|l| l.0 == low) {
                Some(l) => l.1.push(id),
                None => reference.push((low, vec![id])),
            }
        }
        reference.sort_unstable_by_key(|l| l.0);
        let mut want = (Vec::new(), Vec::new());
        for (low, docs) in &reference {
            want.0
                .push(((5 << 16) | u32::from(*low), want.1.len() as u64, docs.len()));
            postings::encode(&mut want.1, docs);
        }
        // Sparse throughout (small batches of a part that stays small),
        // dense from the start, and sparse turning dense.
        let mut small = PostingsPart::default();
        small.add(&pairs[..1000]);
        assert!(small.slots.is_empty());
        let small_want = {
            let mut part = PostingsPart::default();
            part.add(&pairs[..1000]);
            part.slots = vec![0; 1 << 16];
            for (i, l) in part.lists.iter().enumerate() {
                part.slots[usize::from(l.0)] = i as u32 + 1;
            }
            encode_part(5, part)
        };
        assert_eq!(encode_part(5, small), small_want);
        let mut dense = PostingsPart::default();
        dense.add(&pairs);
        assert!(!dense.slots.is_empty());
        assert_eq!(encode_part(5, dense), want);
        let mut turned = PostingsPart::default();
        turned.add(&pairs[..1500]);
        assert!(turned.slots.is_empty());
        turned.add(&pairs[1500..]);
        assert!(!turned.slots.is_empty());
        assert_eq!(encode_part(5, turned), want);
    }

    #[test]
    fn parallel_postings_equal_serial() {
        let docs: Vec<(u32, Vec<u32>)> = (0..400u32)
            .map(|id| {
                let mut ts: Vec<u32> = (0..400u32)
                    .map(|j| (id * 7919 + j * 104_729) % (1 << 24))
                    .collect();
                ts.sort_unstable();
                ts.dedup();
                (id, ts)
            })
            .collect();
        let encode = |threads| {
            let mut parts: Vec<PostingsPart> =
                (0..PARTS).map(|_| PostingsPart::default()).collect();
            for chunk in docs.chunks(150) {
                fill_postings(&mut parts, chunk, threads);
            }
            encode_postings(parts, threads)
        };
        let serial = encode(1);
        assert!(docs.iter().map(|d| d.1.len()).sum::<usize>() >= PARALLEL_MIN);
        assert_eq!(encode(3), serial);
        assert_eq!(encode(8), serial);
    }
}
