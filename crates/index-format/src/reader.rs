//! Reading shards through mmap.

use std::borrow::Cow;
use std::fs::File;
use std::ops::{ControlFlow, Range};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use roaring::RoaringBitmap;

use crate::paths::Paths;
use crate::symbols::SymbolsLayout;
use crate::writer::Codec;
use crate::{
    BranchMeta, DOC_ROW, DocSymbol, FLAG_BINARY, FLAG_TOO_LARGE, FormatError, HEADER_FIXED, MAGIC,
    MIN_VERSION, RepoMeta, Result, SECTIONS, Section, TOC_ENTRY, VERSION, corrupt, postings,
    trigram, varint,
};

enum Data {
    Mmap(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Data {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Data::Mmap(m) => m,
            Data::Owned(v) => v,
        }
    }
}

/// A section of the shared file bytes, for the FST.
#[derive(Clone)]
struct Bytes {
    data: Arc<Data>,
    range: Range<usize>,
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

/// One row of the DOCS section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Doc {
    pub id: u32,
    /// Index into [`Shard::repos`].
    pub repo: u16,
    /// 0 for none, else `Shard::langs()[lang - 1]`.
    pub lang: u16,
    pub flags: u16,
    pub branch_mask: u64,
    /// Index of the path in the PATHS section; versions of a path share it.
    pub path: u32,
    pub size: u32,
    pub line_count: u32,
    pub(crate) content_len: u32,
    pub(crate) content_off: u64,
    pub(crate) newlines_off: u64,
    pub(crate) newlines_len: u32,
    pub oid: [u8; 20],
}

impl Doc {
    /// Whether the content was indexed (not binary, not too large).
    pub fn has_content(&self) -> bool {
        self.flags & (FLAG_BINARY | FLAG_TOO_LARGE) == 0
    }

    fn parse(id: u32, r: &[u8]) -> Doc {
        let u16_at = |o: usize| u16::from_le_bytes([r[o], r[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().expect("4 bytes"));
        let u64_at = |o: usize| u64::from_le_bytes(r[o..o + 8].try_into().expect("8 bytes"));
        Doc {
            id,
            repo: u16_at(0),
            lang: u16_at(2),
            flags: u16_at(4),
            branch_mask: u64_at(8),
            path: u32_at(16),
            size: u32_at(20),
            line_count: u32_at(24),
            content_len: u32_at(28),
            content_off: u64_at(32),
            newlines_off: u64_at(40),
            newlines_len: u32_at(48),
            oid: r[52..72].try_into().expect("20 bytes"),
        }
    }
}

/// An open shard. Cheap to share behind an `Arc`; the mapping lives until
/// the last reference is dropped.
pub struct Shard {
    data: Arc<Data>,
    codec: Codec,
    sections: Vec<Range<usize>>,
    repos: Vec<RepoMeta>,
    langs: Vec<String>,
    doc_count: u32,
    trigrams: fst::Map<Bytes>,
    /// Per repo, its docs' ids (docs are sorted by repo).
    repo_docs: Vec<Range<u32>>,
    symbols: SymbolsLayout,
    id: u32,
    tombstones: Option<(u32, RoaringBitmap)>,
    /// The RANKS section's per-doc ranks, when it has one.
    ranks: Option<Range<usize>>,
    /// Per repository, the generation of its ranks (all 0 without RANKS).
    rank_generations: Vec<u64>,
    /// Docs with content and their total size, computed on first use.
    content_stats: OnceLock<(u64, u64)>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("repos", &self.repos)
            .field("docs", &self.doc_count)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Iterates docs with their paths, in doc order ([`Shard::walk_docs`]).
pub type PathIter<'a> = dyn FnMut(Doc, &str) -> ControlFlow<()> + 'a;

impl Shard {
    /// Maps and validates the shard at `path`.
    pub fn open(path: &Path) -> Result<Shard> {
        let file = File::open(path)?;
        // SAFETY: shards are written to a temporary file and renamed into
        // place, and never modified afterwards, so the mapped bytes do not
        // change while mapped. Every read is bounds-checked.
        #[allow(unsafe_code)]
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Shard::parse(Data::Mmap(map))
    }

    /// Reads a shard held in memory (ephemeral indexes, tests).
    pub fn from_vec(bytes: Vec<u8>) -> Result<Shard> {
        Shard::parse(Data::Owned(bytes))
    }

    fn parse(data: Data) -> Result<Shard> {
        let d: &[u8] = &data;
        if d.len() < HEADER_FIXED || &d[..8] != MAGIC {
            return Err(corrupt("not a shard (bad magic)"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().expect("4 bytes"));
        let version = u32_at(8);
        if !(MIN_VERSION..=VERSION).contains(&version) {
            return Err(FormatError::Version(version));
        }
        let codec = Codec::from_flag(u32_at(12)).ok_or_else(|| corrupt("unknown codec"))?;
        let n = u32_at(16) as usize;
        let header_len = n
            .checked_mul(TOC_ENTRY)
            .and_then(|t| t.checked_add(HEADER_FIXED + 4))
            .filter(|&l| l <= d.len())
            .ok_or_else(|| corrupt("section table truncated"))?;
        if crc32fast::hash(&d[..header_len - 4]) != u32_at(header_len - 4) {
            return Err(corrupt("header checksum mismatch"));
        }
        let id = u32_at(header_len - 4);
        let mut sections = vec![0..0; SECTIONS.len()];
        let mut seen = [false; SECTIONS.len()];
        let mut tombstones = None;
        let mut ranks_section = None;
        for i in 0..n {
            let e = &d[HEADER_FIXED + i * TOC_ENTRY..][..TOC_ENTRY];
            let kind = u32::from_le_bytes(e[0..4].try_into().expect("4 bytes"));
            let off = u64::from_le_bytes(e[8..16].try_into().expect("8 bytes"));
            let len = u64::from_le_bytes(e[16..24].try_into().expect("8 bytes"));
            let crc = u32::from_le_bytes(e[24..28].try_into().expect("4 bytes"));
            let range = usize::try_from(off)
                .ok()
                .zip(usize::try_from(len).ok())
                .and_then(|(o, l)| Some(o..o.checked_add(l)?))
                .filter(|r| r.end <= d.len() && r.start >= header_len)
                .ok_or_else(|| corrupt(format!("section {kind} out of bounds")))?;
            if crc32fast::hash(&d[range.clone()]) != crc {
                return Err(corrupt(format!("section {kind} checksum mismatch")));
            }
            if kind == Section::Tombstones as u32 {
                let body = &d[range];
                let base = body
                    .get(..4)
                    .ok_or_else(|| corrupt("tombstones truncated"))?;
                let base = u32::from_le_bytes(base.try_into().expect("4 bytes"));
                let docs = RoaringBitmap::deserialize_from(&body[4..])
                    .map_err(|e| corrupt(format!("tombstones: {e}")))?;
                tombstones = Some((base, docs));
                continue;
            }
            if kind == Section::Ranks as u32 {
                ranks_section = Some(range);
                continue;
            }
            // Unknown sections are skipped: newer writers may add some.
            if let Some(idx) = SECTIONS.iter().position(|s| *s as u32 == kind) {
                sections[idx] = range;
                seen[idx] = true;
            }
        }
        if let Some(i) = seen.iter().position(|s| !s) {
            return Err(corrupt(format!("missing section {:?}", SECTIONS[i])));
        }
        let sec = |s: Section| &d[sections[s.slot()].clone()];

        let repos = parse_repos(sec(Section::Repos))?;
        let langs = parse_strings(sec(Section::Langs))?;
        let docs = sec(Section::Docs);
        if docs.len() % DOC_ROW != 0 {
            return Err(corrupt("docs section length"));
        }
        let doc_count =
            u32::try_from(docs.len() / DOC_ROW).map_err(|_| corrupt("too many docs"))?;
        let paths = Paths::parse(sec(Section::Paths))?;
        let content_len = sec(Section::Content).len() as u64;
        let newlines_len = sec(Section::Newlines).len() as u64;
        let mut prev_path = 0u32;
        let mut repo_docs = vec![0..0; repos.len()];
        let mut prev_repo = 0u16;
        for (i, row) in docs.chunks_exact(DOC_ROW).enumerate() {
            let doc = Doc::parse(i as u32, row);
            let repo = repos
                .get(usize::from(doc.repo))
                .ok_or_else(|| corrupt(format!("doc {i}: repo out of range")))?;
            let ok = usize::from(doc.lang) <= langs.len()
                && doc.repo >= prev_repo
                && doc.path < paths.count
                && doc.path >= prev_path
                && (repo.branches.len() >= 64 || doc.branch_mask >> repo.branches.len() == 0)
                && doc
                    .content_off
                    .checked_add(u64::from(doc.content_len))
                    .is_some_and(|e| e <= content_len)
                && doc
                    .newlines_off
                    .checked_add(u64::from(doc.newlines_len))
                    .is_some_and(|e| e <= newlines_len);
            if !ok {
                return Err(corrupt(format!("doc {i}: reference out of range")));
            }
            prev_path = doc.path;
            let r = &mut repo_docs[usize::from(doc.repo)];
            if r.start == r.end {
                *r = i as u32..i as u32;
            }
            r.end = i as u32 + 1;
            prev_repo = doc.repo;
        }
        let symbols = SymbolsLayout::parse(sec(Section::Symbols), doc_count)?;
        let (ranks, rank_generations) = match ranks_section {
            None => (None, vec![0; repos.len()]),
            Some(r) => parse_ranks(d, r, repos.len(), doc_count)?,
        };
        let tri = sections[Section::Trigrams.slot()].clone();
        let data = Arc::new(data);
        let trigrams = fst::Map::new(Bytes {
            data: data.clone(),
            range: tri,
        })
        .map_err(|e| corrupt(format!("trigram FST: {e}")))?;
        Ok(Shard {
            data,
            codec,
            sections,
            repos,
            langs,
            doc_count,
            trigrams,
            repo_docs,
            symbols,
            id,
            tombstones,
            ranks,
            rank_generations,
            content_stats: OnceLock::new(),
        })
    }

    /// The shard's id: its header checksum, which covers every section's
    /// checksum.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// For a delta shard, the id of its base and the base docs it hides.
    pub fn tombstones(&self) -> Option<(u32, &RoaringBitmap)> {
        self.tombstones.as_ref().map(|(b, d)| (*b, d))
    }

    fn section(&self, s: Section) -> &[u8] {
        &self.data[self.sections[s.slot()].clone()]
    }

    /// File size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn repos(&self) -> &[RepoMeta] {
        &self.repos
    }

    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    /// The ids of the docs of repository `repo` (an index into
    /// [`Shard::repos`]); empty when it has none. Panics if out of range.
    pub fn repo_docs(&self, repo: u16) -> Range<u32> {
        self.repo_docs[usize::from(repo)].clone()
    }

    /// The number of docs with content and their total size in bytes
    /// (for BM25 ranking), computed once.
    pub fn content_stats(&self) -> (u64, u64) {
        *self.content_stats.get_or_init(|| {
            self.docs()
                .filter(Doc::has_content)
                .fold((0, 0), |(n, bytes), d| (n + 1, bytes + u64::from(d.size)))
        })
    }

    /// The doc's static rank in [0, 1]; 0 without ranks. Panics
    /// if `id >= doc_count()`.
    pub fn doc_rank(&self, id: u32) -> f32 {
        assert!(id < self.doc_count, "doc {id} out of range");
        let Some(r) = &self.ranks else {
            return 0.0;
        };
        let at = r.start + 4 * id as usize;
        let v = f32::from_le_bytes(self.data[at..at + 4].try_into().expect("4 bytes"));
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// The generation of the ranks of repository `repo` (an index into
    /// [`Shard::repos`]); 0 when it has none. Panics if out of range.
    pub fn ranks_generation(&self, repo: u16) -> u64 {
        self.rank_generations[usize::from(repo)]
    }

    /// The doc with id `id`. Panics if `id >= doc_count()`.
    pub fn doc(&self, id: u32) -> Doc {
        let start = id as usize * DOC_ROW;
        Doc::parse(id, &self.section(Section::Docs)[start..start + DOC_ROW])
    }

    pub fn docs(&self) -> impl Iterator<Item = Doc> + '_ {
        self.section(Section::Docs)
            .chunks_exact(DOC_ROW)
            .enumerate()
            .map(|(i, r)| Doc::parse(i as u32, r))
    }

    /// The doc's language id.
    pub fn lang(&self, doc: &Doc) -> Option<&str> {
        let i = usize::from(doc.lang).checked_sub(1)?;
        self.langs.get(i).map(String::as_str)
    }

    /// The language ids docs refer to.
    pub fn langs(&self) -> &[String] {
        &self.langs
    }

    fn paths(&self) -> Paths<'_> {
        Paths::parse(self.section(Section::Paths)).expect("validated at open")
    }

    pub fn path(&self, doc: &Doc) -> Result<String> {
        String::from_utf8(self.paths().get(doc.path)?).map_err(|_| corrupt("path is not UTF-8"))
    }

    /// Calls `f` with each doc from `from` on and its path, in doc order.
    pub fn walk_docs(&self, from: u32, f: &mut PathIter<'_>) -> Result<()> {
        if from >= self.doc_count {
            return Ok(());
        }
        let mut next = from;
        let mut doc = self.doc(next);
        let mut err = None;
        self.paths().walk(doc.path, |i, p| {
            let Ok(p) = std::str::from_utf8(p) else {
                err = Some(corrupt("path is not UTF-8"));
                return ControlFlow::Break(());
            };
            while doc.path == i {
                f(doc, p)?;
                next += 1;
                if next == self.doc_count {
                    return ControlFlow::Break(());
                }
                doc = self.doc(next);
            }
            ControlFlow::Continue(())
        })?;
        err.map_or(Ok(()), Err)
    }

    /// The doc's content; empty for docs without content.
    pub fn content(&self, doc: &Doc) -> Result<Cow<'_, [u8]>> {
        if !doc.has_content() {
            return Ok(Cow::Borrowed(&[]));
        }
        let off = doc.content_off as usize;
        let stored = &self.section(Section::Content)[off..off + doc.content_len as usize];
        match self.codec {
            Codec::None => Ok(Cow::Borrowed(stored)),
            Codec::Lz4 => lz4_flex::block::decompress(stored, doc.size as usize)
                .map(Cow::Owned)
                .map_err(|e| corrupt(format!("doc {}: {e}", doc.id))),
        }
    }

    /// The doc's stored content and encoded newlines, as written.
    pub(crate) fn raw_doc(&self, doc: &Doc) -> (&[u8], &[u8]) {
        let off = doc.content_off as usize;
        let stored = &self.section(Section::Content)[off..off + doc.content_len as usize];
        let off = doc.newlines_off as usize;
        let newlines = &self.section(Section::Newlines)[off..off + doc.newlines_len as usize];
        (stored, newlines)
    }

    /// The symbols defined in the doc, in definition order.
    pub fn symbols(&self, doc: &Doc) -> Result<Vec<DocSymbol>> {
        self.symbols
            .read(self.section(Section::Symbols), doc.id, doc.size)
    }

    /// The kinds and name ranges of the doc's symbols, in stored order;
    /// cheaper than [`Shard::symbols`], which also decodes the names.
    pub fn symbol_ranges(&self, doc: &Doc) -> Result<Vec<(u8, Range<u32>)>> {
        self.symbols
            .read_ranges(self.section(Section::Symbols), doc.id, doc.size)
    }

    /// The version of the symbol extractor the symbols come from; 0 when
    /// none were extracted.
    pub fn symbols_version(&self) -> u32 {
        self.symbols.version
    }

    /// Whether the doc has symbols.
    pub fn has_symbols(&self, doc: &Doc) -> bool {
        self.symbols
            .has_symbols(self.section(Section::Symbols), doc.id)
    }

    /// Byte offsets of the doc's newlines.
    pub fn newlines(&self, doc: &Doc) -> Result<Vec<u32>> {
        let off = doc.newlines_off as usize;
        let data = &self.section(Section::Newlines)[off..off + doc.newlines_len as usize];
        let mut out = Vec::new();
        let mut pos = 0;
        let mut cur = 0u64;
        while pos < data.len() {
            cur += varint::get(data, &mut pos)?;
            out.push(u32::try_from(cur).map_err(|_| corrupt("newline offset"))?);
        }
        Ok(out)
    }

    /// The number of docs holding the trigram `t` (see [`trigram_bytes`](crate::trigram_bytes)).
    pub fn trigram_count(&self, t: u32) -> Result<u64> {
        let Some(v) = self.trigrams.get(trigram::key(t)) else {
            return Ok(0);
        };
        let (off, count) = postings::split_fst_value(v);
        if count < postings::COUNT_MAX {
            return Ok(count);
        }
        postings::count(self.section(Section::Postings), off as usize)
    }

    /// The docs holding the trigram `t`.
    pub fn postings(&self, t: u32) -> Result<RoaringBitmap> {
        let Some(v) = self.trigrams.get(trigram::key(t)) else {
            return Ok(RoaringBitmap::new());
        };
        let (off, _) = postings::split_fst_value(v);
        let data = self.section(Section::Postings);
        if off as usize >= data.len() {
            return Err(corrupt("postings offset"));
        }
        postings::decode(data, off as usize, self.doc_count)
    }

    /// The number of distinct trigrams.
    pub fn trigram_total(&self) -> usize {
        self.trigrams.len()
    }
}

/// The RANKS section at `range` of `d`: the range of its per-doc ranks and
/// the generation per repository.
fn parse_ranks(
    d: &[u8],
    range: Range<usize>,
    repos: usize,
    docs: u32,
) -> Result<(Option<Range<usize>>, Vec<u64>)> {
    let body = &d[range.clone()];
    let mut pos = 0;
    let n = get_u32(body, &mut pos)? as usize;
    if n != repos || body.len() != 4 + 8 * n + 4 * docs as usize {
        return Err(corrupt("ranks section length"));
    }
    let generations = (0..n)
        .map(|i| {
            let at = 4 + 8 * i;
            u64::from_le_bytes(body[at..at + 8].try_into().expect("8 bytes"))
        })
        .collect();
    Ok((Some(range.start + 4 + 8 * n..range.end), generations))
}

fn parse_strings(s: &[u8]) -> Result<Vec<String>> {
    let mut pos = 0;
    let n = get_u32(s, &mut pos)?;
    (0..n).map(|_| get_str(s, &mut pos)).collect()
}

fn parse_repos(s: &[u8]) -> Result<Vec<RepoMeta>> {
    let mut pos = 0;
    let n = get_u32(s, &mut pos)?;
    let mut out = Vec::new();
    for _ in 0..n {
        let id = get_u32(s, &mut pos)? as i32;
        let rank = get_u16(s, &mut pos)?;
        let nb = get_u16(s, &mut pos)?;
        if nb > 64 {
            return Err(corrupt("more than 64 branches"));
        }
        let name = get_str(s, &mut pos)?;
        let mut branches = Vec::new();
        for _ in 0..nb {
            let name = get_str(s, &mut pos)?;
            let commit = take(s, &mut pos, 20)?.try_into().expect("20 bytes");
            branches.push(BranchMeta { name, commit });
        }
        out.push(RepoMeta {
            id,
            name,
            rank,
            branches,
        });
    }
    Ok(out)
}

fn take<'a>(s: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let b = s
        .get(*pos..pos.saturating_add(n))
        .ok_or_else(|| corrupt("metadata truncated"))?;
    *pos += n;
    Ok(b)
}

fn get_u16(s: &[u8], pos: &mut usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        take(s, pos, 2)?.try_into().expect("2 bytes"),
    ))
}

fn get_u32(s: &[u8], pos: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take(s, pos, 4)?.try_into().expect("4 bytes"),
    ))
}

fn get_str(s: &[u8], pos: &mut usize) -> Result<String> {
    let n = get_u32(s, pos)? as usize;
    String::from_utf8(take(s, pos, n)?.to_vec()).map_err(|_| corrupt("string is not UTF-8"))
}
