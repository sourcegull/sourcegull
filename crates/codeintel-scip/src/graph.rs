//! Code graph files: the immutable, per-upload result of processing a SCIP index.
//!
//! Layout (little-endian):
//!
//! ```text
//! "SKCGRAPH" u32 version u32 0
//! DOCS          lz4 document chunks, written while the index streams in
//! DOC_CHUNKS    per chunk: u64 offset, u32 len (a path split over several SCIP documents
//!               has several chunks)
//! DOC_STARTS    per document: u32 index of its first chunk, plus one final entry
//! DOC_PATHS     table: document id → repository-relative path
//! PATHS_FST     fst: path → document id
//! SYM_NAMES     table: symbol id → symbol string
//! SYMBOLS_FST   fst: symbol string → symbol id
//! SYM_RECORDS   table: symbol id → record (info, implementations, postings)
//! TEXTS         table: text id → hover text (documentation deduplicated by hash)
//! footer        per section: u64 offset, u64 len; u32 crc32 of those; "SKCGEND1"
//! ```
//!
//! A table is `u64 count`, `count + 1` u64 offsets relative to its data, then the data.
//! Integers inside records are LEB128 varints. Document chunks hold occurrences sorted by
//! range with delta-encoded positions and symbols as ids into SYM_NAMES.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, Write};
use std::ops::Range as StdRange;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::proto;
use crate::symbol::{Package, is_local, package, symbol_hash};

const MAGIC: &[u8; 8] = b"SKCGRAPH";
const END_MAGIC: &[u8; 8] = b"SKCGEND1";
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 16;

const DOCS: usize = 0;
const DOC_CHUNKS: usize = 1;
const DOC_STARTS: usize = 2;
const DOC_PATHS: usize = 3;
const PATHS_FST: usize = 4;
const SYM_NAMES: usize = 5;
const SYMBOLS_FST: usize = 6;
const SYM_RECORDS: usize = 7;
const TEXTS: usize = 8;
const NUM_SECTIONS: usize = 9;
const FOOTER_LEN: usize = NUM_SECTIONS * 16 + 4 + 8;

/// Posting flags: what a document holds for a symbol.
pub mod posting {
    /// A definition occurrence.
    pub const DEFINITION: u8 = 1;
    /// A non-definition occurrence.
    pub const REFERENCE: u8 = 2;
    /// The definition of a symbol that implements this one.
    pub const IMPLEMENTATION: u8 = 4;
}

/// Relationship flags.
pub mod relation {
    pub const REFERENCE: u8 = 1;
    pub const IMPLEMENTATION: u8 = 2;
    pub const TYPE_DEFINITION: u8 = 4;
    pub const DEFINITION: u8 = 8;
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("corrupt code graph file: {0}")]
    Corrupt(String),
}

fn corrupt(what: impl Into<String>) -> GraphError {
    GraphError::Corrupt(what.into())
}

pub type Result<T, E = GraphError> = std::result::Result<T, E>;

/// A range in a document, zero-based, end exclusive, in the document's position encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Range {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl Range {
    fn from4(r: [u32; 4]) -> Self {
        Self {
            start_line: r[0],
            start_character: r[1],
            end_line: r[2],
            end_character: r[3],
        }
    }

    /// Whether the position lies in the range (an empty range contains its start).
    pub fn contains(&self, line: u32, character: u32) -> bool {
        let p = (line, character);
        let start = (self.start_line, self.start_character);
        let end = (self.end_line, self.end_character);
        start <= p && (p < end || start == end && p == start)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diagnostic {
    pub severity: u32,
    pub code: String,
    pub message: String,
    pub source: String,
    pub tags: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Occurrence {
    pub range: Range,
    /// Symbol id ([`CodeGraph::symbol_name`]).
    pub symbol: Option<u32>,
    /// `SymbolRole` bits ([`crate::proto::role`]).
    pub roles: u32,
    pub syntax_kind: u32,
    pub override_documentation: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub enclosing_range: Option<Range>,
}

impl Occurrence {
    pub fn is_definition(&self) -> bool {
        self.roles & proto::role::DEFINITION as u32 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    pub path: String,
    pub language: String,
    /// `PositionEncoding` ([`crate::proto::encoding`]).
    pub position_encoding: u32,
    /// Sorted by range.
    pub occurrences: Vec<Occurrence>,
}

/// A document holding a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    pub document: u32,
    /// [`posting`] flags.
    pub flags: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub symbol: u32,
    /// [`relation`] flags.
    pub flags: u8,
}

/// `SymbolInformation` of a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SymbolInfo {
    pub kind: u32,
    pub display_name: String,
    pub documentation: Vec<String>,
    pub signature: Option<String>,
    pub enclosing_symbol: Option<u32>,
    pub relationships: Vec<Relationship>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub id: u32,
    pub name: String,
    pub info: Option<SymbolInfo>,
    /// Symbols with an implementation relationship to this one.
    pub implementations: Vec<u32>,
    /// Documents holding the symbol, by document id.
    pub postings: Vec<Posting>,
}

/// Whether a package's symbols are defined by the upload or only used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MonikerKind {
    Export,
    Import,
}

impl MonikerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}

/// What the database needs from a processed upload.
#[derive(Debug, Clone, Default)]
pub struct GraphSummary {
    pub num_documents: u32,
    pub num_occurrences: u64,
    pub num_symbols: u32,
    /// Global symbols that occur in the upload: (symbol hash, union of [`posting`] flags).
    pub postings: Vec<(i64, u8)>,
    /// Packages defined (export) or only used (import) by the upload.
    pub packages: Vec<(MonikerKind, Package)>,
    pub size: u64,
}

// ---------------------------------------------------------------------------------------------
// Varints

fn put_uv(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_uv(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn uv(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&b, rest) = self
                .buf
                .split_first()
                .ok_or_else(|| corrupt("truncated varint"))?;
            self.buf = rest;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("overlong varint"))
    }

    fn u32(&mut self) -> Result<u32> {
        u32::try_from(self.uv()?).map_err(|_| corrupt("value out of range"))
    }

    fn len(&mut self) -> Result<usize> {
        let n = usize::try_from(self.uv()?).map_err(|_| corrupt("length out of range"))?;
        if n > self.buf.len() {
            return Err(corrupt("length past the end"));
        }
        Ok(n)
    }

    /// A count of items each at least one byte long.
    fn count(&mut self) -> Result<usize> {
        self.len()
    }

    fn str(&mut self) -> Result<&'a str> {
        let n = self.len()?;
        let (s, rest) = self.buf.split_at(n);
        self.buf = rest;
        std::str::from_utf8(s).map_err(|_| corrupt("invalid UTF-8"))
    }
}

// ---------------------------------------------------------------------------------------------
// Writer

#[derive(Default)]
struct SymAcc {
    info: Option<InfoAcc>,
    /// Defined by the upload: a definition occurrence or information from a document.
    defined: bool,
    implementations: Vec<u32>,
    postings: Vec<(u32, u8)>,
}

struct InfoAcc {
    kind: u32,
    display_name: String,
    documentation: Vec<u32>,
    signature: Option<u32>,
    enclosing: Option<u32>,
    relationships: Vec<(u32, u8)>,
}

/// Writes a code graph file document by document.
pub struct GraphWriter {
    out: BufWriter<File>,
    pos: u64,
    texts: BufWriter<File>,
    texts_len: u64,
    text_offsets: Vec<u64>,
    text_ids: HashMap<[u8; 16], u32>,
    paths: HashMap<String, u32>,
    doc_paths: Vec<String>,
    doc_chunks: Vec<Vec<(u64, u32)>>,
    symbols: HashMap<Box<str>, u32>,
    syms: Vec<SymAcc>,
    num_occurrences: u64,
    scratch: Vec<u8>,
}

impl GraphWriter {
    /// Creates `path`; temporary data goes to `tmp_dir`.
    pub fn create(path: &Path, tmp_dir: &Path) -> Result<Self> {
        let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            out,
            pos: HEADER_LEN,
            texts: BufWriter::with_capacity(1 << 20, tempfile::tempfile_in(tmp_dir)?),
            texts_len: 0,
            text_offsets: vec![0],
            text_ids: HashMap::new(),
            paths: HashMap::new(),
            doc_paths: Vec::new(),
            doc_chunks: Vec::new(),
            symbols: HashMap::new(),
            syms: Vec::new(),
            num_occurrences: 0,
            scratch: Vec::new(),
        })
    }

    fn intern(&mut self, symbol: &str) -> u32 {
        if let Some(&id) = self.symbols.get(symbol) {
            return id;
        }
        let id = self.syms.len() as u32;
        self.symbols.insert(symbol.into(), id);
        self.syms.push(SymAcc::default());
        id
    }

    fn text(&mut self, text: &str) -> Result<u32> {
        let digest = Sha256::digest(text.as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        if let Some(&id) = self.text_ids.get(&key) {
            return Ok(id);
        }
        self.texts.write_all(text.as_bytes())?;
        self.texts_len += text.len() as u64;
        let id = (self.text_offsets.len() - 1) as u32;
        self.text_offsets.push(self.texts_len);
        self.text_ids.insert(key, id);
        Ok(id)
    }

    fn add_info(&mut self, info: &proto::SymbolInformation, defined: bool) -> Result<()> {
        if info.symbol.is_empty() {
            return Ok(());
        }
        let id = self.intern(&info.symbol);
        let replace = match &self.syms[id as usize].info {
            None => true,
            Some(old) => old.documentation.is_empty() && !info.documentation.is_empty(),
        };
        if defined {
            self.syms[id as usize].defined = true;
        }
        if !replace {
            return Ok(());
        }
        let documentation = info
            .documentation
            .iter()
            .filter(|d| !d.is_empty())
            .map(|d| self.text(d))
            .collect::<Result<Vec<_>>>()?;
        let signature = match &info.signature_documentation {
            Some(s) if !s.text.is_empty() => Some(self.text(&s.text)?),
            _ => None,
        };
        let enclosing =
            (!info.enclosing_symbol.is_empty()).then(|| self.intern(&info.enclosing_symbol));
        let mut relationships = Vec::with_capacity(info.relationships.len());
        for r in &info.relationships {
            if r.symbol.is_empty() {
                continue;
            }
            let flags = [
                (r.is_reference, relation::REFERENCE),
                (r.is_implementation, relation::IMPLEMENTATION),
                (r.is_type_definition, relation::TYPE_DEFINITION),
                (r.is_definition, relation::DEFINITION),
            ]
            .into_iter()
            .filter(|(set, _)| *set)
            .fold(0, |f, (_, bit)| f | bit);
            let target = self.intern(&r.symbol);
            if r.is_implementation && !self.syms[target as usize].implementations.contains(&id) {
                self.syms[target as usize].implementations.push(id);
            }
            relationships.push((target, flags));
        }
        self.syms[id as usize].info = Some(InfoAcc {
            kind: u32::try_from(info.kind).unwrap_or(0),
            display_name: info.display_name.clone(),
            documentation,
            signature,
            enclosing,
            relationships,
        });
        Ok(())
    }

    /// Adds `external_symbols` information (symbols the upload uses but does not define).
    pub fn add_external_symbol(&mut self, info: &proto::SymbolInformation) -> Result<()> {
        self.add_info(info, false)
    }

    /// Adds a document at the repository-relative `path`; returns its occurrence count.
    /// A path seen before gets another chunk.
    pub fn add_document(&mut self, path: &str, doc: &proto::Document) -> Result<u64> {
        let doc_id = match self.paths.get(path) {
            Some(&id) => id,
            None => {
                let id = self.doc_paths.len() as u32;
                self.paths.insert(path.to_owned(), id);
                self.doc_paths.push(path.to_owned());
                self.doc_chunks.push(Vec::new());
                id
            }
        };
        for info in &doc.symbols {
            self.add_info(info, true)?;
            // The definition of an implementation is in this document.
            if let Some(&id) = self.symbols.get(info.symbol.as_str()) {
                let targets: Vec<u32> = self.syms[id as usize]
                    .info
                    .iter()
                    .flat_map(|i| &i.relationships)
                    .filter(|(_, f)| f & relation::IMPLEMENTATION != 0)
                    .map(|(t, _)| *t)
                    .collect();
                for t in targets {
                    self.post(t, doc_id, posting::IMPLEMENTATION);
                }
            }
        }

        struct Occ {
            range: Range,
            symbol: Option<u32>,
            roles: u32,
            syntax_kind: u32,
            overrides: Vec<u32>,
            diagnostics: Vec<Diagnostic>,
            enclosing: Option<Range>,
        }
        let mut occs = Vec::with_capacity(doc.occurrences.len());
        for o in &doc.occurrences {
            let Some(r) = o.range4() else { continue };
            let symbol = (!o.symbol.is_empty()).then(|| self.intern(&o.symbol));
            let roles = o.symbol_roles as u32;
            if let Some(s) = symbol {
                let flag = if roles & proto::role::DEFINITION as u32 != 0 {
                    self.syms[s as usize].defined = true;
                    posting::DEFINITION
                } else {
                    posting::REFERENCE
                };
                self.post(s, doc_id, flag);
            }
            let overrides = o
                .override_documentation
                .iter()
                .filter(|d| !d.is_empty())
                .map(|d| self.text(d))
                .collect::<Result<Vec<_>>>()?;
            occs.push(Occ {
                range: Range::from4(r),
                symbol,
                roles,
                syntax_kind: u32::try_from(o.syntax_kind).unwrap_or(0),
                overrides,
                diagnostics: o
                    .diagnostics
                    .iter()
                    .map(|d| Diagnostic {
                        severity: u32::try_from(d.severity).unwrap_or(0),
                        code: d.code.clone(),
                        message: d.message.clone(),
                        source: d.source.clone(),
                        tags: d.tags.iter().map(|&t| t as u32).collect(),
                    })
                    .collect(),
                enclosing: o.enclosing4().map(Range::from4),
            });
        }
        occs.sort_by_key(|o| (o.range, o.symbol));
        // Merge occurrences of the same symbol at the same range.
        let mut merged: Vec<Occ> = Vec::with_capacity(occs.len());
        for o in occs {
            match merged.last_mut() {
                Some(last) if last.range == o.range && last.symbol == o.symbol => {
                    last.roles |= o.roles;
                    if last.overrides.is_empty() {
                        last.overrides = o.overrides;
                    }
                    last.diagnostics.extend(o.diagnostics);
                    last.enclosing = last.enclosing.or(o.enclosing);
                }
                _ => merged.push(o),
            }
        }

        let buf = &mut self.scratch;
        buf.clear();
        put_uv(buf, u64::try_from(doc.position_encoding).unwrap_or(0));
        put_str(buf, &doc.language);
        put_uv(buf, merged.len() as u64);
        let (mut line, mut ch) = (0u32, 0u32);
        for o in &merged {
            let r = o.range;
            put_uv(buf, u64::from(r.start_line - line));
            if r.start_line == line {
                put_uv(buf, u64::from(r.start_character - ch));
            } else {
                put_uv(buf, u64::from(r.start_character));
            }
            (line, ch) = (r.start_line, r.start_character);
            put_uv(buf, u64::from(r.end_line - r.start_line));
            put_uv(buf, u64::from(r.end_character));
            put_uv(buf, o.symbol.map_or(0, |s| u64::from(s) + 1));
            put_uv(buf, u64::from(o.roles));
            put_uv(buf, u64::from(o.syntax_kind));
            let flags = u64::from(!o.overrides.is_empty())
                | u64::from(!o.diagnostics.is_empty()) << 1
                | u64::from(o.enclosing.is_some()) << 2;
            put_uv(buf, flags);
            if !o.overrides.is_empty() {
                put_uv(buf, o.overrides.len() as u64);
                for &t in &o.overrides {
                    put_uv(buf, u64::from(t));
                }
            }
            if !o.diagnostics.is_empty() {
                put_uv(buf, o.diagnostics.len() as u64);
                for d in &o.diagnostics {
                    put_uv(buf, u64::from(d.severity));
                    put_str(buf, &d.code);
                    put_str(buf, &d.message);
                    put_str(buf, &d.source);
                    put_uv(buf, d.tags.len() as u64);
                    for &t in &d.tags {
                        put_uv(buf, u64::from(t));
                    }
                }
            }
            if let Some(e) = o.enclosing {
                for v in [e.start_line, e.start_character, e.end_line, e.end_character] {
                    put_uv(buf, u64::from(v));
                }
            }
        }
        let compressed = lz4_flex::compress_prepend_size(buf);
        let len =
            u32::try_from(compressed.len()).map_err(|_| io::Error::other("document too large"))?;
        self.out.write_all(&compressed)?;
        self.doc_chunks[doc_id as usize].push((self.pos, len));
        self.pos += u64::from(len);
        self.num_occurrences += merged.len() as u64;
        Ok(merged.len() as u64)
    }

    fn post(&mut self, symbol: u32, doc: u32, flag: u8) {
        let postings = &mut self.syms[symbol as usize].postings;
        match postings.last_mut() {
            Some((d, f)) if *d == doc => *f |= flag,
            _ => postings.push((doc, flag)),
        }
    }

    fn write_section(&mut self, data: &[u8]) -> Result<(u64, u64)> {
        let start = self.pos;
        self.out.write_all(data)?;
        self.pos += data.len() as u64;
        Ok((start, data.len() as u64))
    }

    /// Writes the indexes and the footer, and returns what the database needs.
    pub fn finish(mut self) -> Result<GraphSummary> {
        let mut sections = [(0u64, 0u64); NUM_SECTIONS];
        sections[DOCS] = (HEADER_LEN, self.pos - HEADER_LEN);

        let mut chunks = Vec::new();
        let mut starts = Vec::with_capacity((self.doc_chunks.len() + 1) * 4);
        let mut n = 0u32;
        for doc in &self.doc_chunks {
            starts.extend_from_slice(&n.to_le_bytes());
            for &(off, len) in doc {
                chunks.extend_from_slice(&off.to_le_bytes());
                chunks.extend_from_slice(&len.to_le_bytes());
                n += 1;
            }
        }
        starts.extend_from_slice(&n.to_le_bytes());
        sections[DOC_CHUNKS] = self.write_section(&chunks)?;
        sections[DOC_STARTS] = self.write_section(&starts)?;
        drop((chunks, starts));

        let paths = std::mem::take(&mut self.doc_paths);
        sections[DOC_PATHS] = self.write_section(&table(paths.iter().map(|p| p.as_bytes())))?;
        let mut sorted: Vec<(&str, u32)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i as u32))
            .collect();
        sorted.sort_unstable();
        let fst = fst::Map::from_iter(sorted.iter().map(|&(p, i)| (p, u64::from(i))))
            .map_err(|e| io::Error::other(e.to_string()))?;
        sections[PATHS_FST] = self.write_section(fst.as_fst().as_bytes())?;
        drop(fst);

        let symbols = std::mem::take(&mut self.symbols);
        let mut names: Vec<Option<Box<str>>> = vec![None; self.syms.len()];
        for (name, id) in symbols {
            names[id as usize] = Some(name);
        }
        let names: Vec<Box<str>> = names.into_iter().map(Option::unwrap_or_default).collect();
        sections[SYM_NAMES] = self.write_section(&table(names.iter().map(|n| n.as_bytes())))?;
        let mut sorted: Vec<(&str, u32)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (&**n, i as u32))
            .collect();
        sorted.sort_unstable();
        let fst = fst::Map::from_iter(sorted.iter().map(|&(n, i)| (n, u64::from(i))))
            .map_err(|e| io::Error::other(e.to_string()))?;
        drop(sorted);
        sections[SYMBOLS_FST] = self.write_section(fst.as_fst().as_bytes())?;
        drop(fst);

        let mut summary = GraphSummary {
            num_documents: paths.len() as u32,
            num_occurrences: self.num_occurrences,
            num_symbols: names.len() as u32,
            ..Default::default()
        };
        let mut packages: HashMap<Package, bool> = HashMap::new();
        let syms = std::mem::take(&mut self.syms);
        let mut records = Vec::new();
        let mut offsets = Vec::with_capacity(syms.len() + 1);
        for (id, mut s) in syms.into_iter().enumerate() {
            offsets.push(records.len() as u64);
            s.postings.sort_unstable();
            s.postings.dedup_by(|b, a| {
                if a.0 == b.0 {
                    a.1 |= b.1;
                    true
                } else {
                    false
                }
            });
            let name = &names[id];
            if !is_local(name) && (!s.postings.is_empty() || s.defined) {
                let flags = s.postings.iter().fold(0u8, |f, p| f | p.1);
                if flags != 0 {
                    summary.postings.push((symbol_hash(name), flags));
                }
                if let Some(p) = package(name) {
                    *packages.entry(p).or_default() |= s.defined;
                }
            }
            encode_symbol(&mut records, &s);
        }
        offsets.push(records.len() as u64);
        let mut data = Vec::with_capacity(8 * (offsets.len() + 1) + records.len());
        data.extend_from_slice(&((offsets.len() - 1) as u64).to_le_bytes());
        for o in &offsets {
            data.extend_from_slice(&o.to_le_bytes());
        }
        data.extend_from_slice(&records);
        drop(records);
        sections[SYM_RECORDS] = self.write_section(&data)?;
        drop(data);

        let mut head = Vec::with_capacity(8 * (self.text_offsets.len() + 1));
        head.extend_from_slice(&((self.text_offsets.len() - 1) as u64).to_le_bytes());
        for o in &self.text_offsets {
            head.extend_from_slice(&o.to_le_bytes());
        }
        let start = self.pos;
        self.write_section(&head)?;
        let mut texts = self.texts.into_inner().map_err(|e| e.into_error())?;
        texts.rewind()?;
        let copied = io::copy(&mut texts, &mut self.out)?;
        self.pos += copied;
        sections[TEXTS] = (start, self.pos - start);

        let mut footer = Vec::with_capacity(FOOTER_LEN);
        for (off, len) in sections {
            footer.extend_from_slice(&off.to_le_bytes());
            footer.extend_from_slice(&len.to_le_bytes());
        }
        let crc = crc32fast::hash(&footer);
        footer.extend_from_slice(&crc.to_le_bytes());
        footer.extend_from_slice(END_MAGIC);
        self.out.write_all(&footer)?;
        self.pos += footer.len() as u64;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        summary.size = self.pos;

        summary.postings.sort_unstable();
        summary.postings.dedup_by(|b, a| {
            if a.0 == b.0 {
                a.1 |= b.1;
                true
            } else {
                false
            }
        });
        summary.packages = packages
            .into_iter()
            .map(|(p, defined)| {
                let kind = if defined {
                    MonikerKind::Export
                } else {
                    MonikerKind::Import
                };
                (kind, p)
            })
            .collect();
        summary.packages.sort();
        Ok(summary)
    }
}

/// `u64 count`, `count + 1` offsets, data.
fn table<'a>(items: impl ExactSizeIterator<Item = &'a [u8]> + Clone) -> Vec<u8> {
    let n = items.len();
    let total: usize = items.clone().map(<[u8]>::len).sum();
    let mut out = Vec::with_capacity(8 * (n + 2) + total);
    out.extend_from_slice(&(n as u64).to_le_bytes());
    let mut off = 0u64;
    out.extend_from_slice(&off.to_le_bytes());
    for item in items.clone() {
        off += item.len() as u64;
        out.extend_from_slice(&off.to_le_bytes());
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn encode_symbol(out: &mut Vec<u8>, s: &SymAcc) {
    match &s.info {
        None => put_uv(out, 0),
        Some(i) => {
            put_uv(out, 1);
            put_uv(out, u64::from(i.kind));
            put_str(out, &i.display_name);
            put_uv(out, i.documentation.len() as u64);
            for &d in &i.documentation {
                put_uv(out, u64::from(d));
            }
            put_uv(out, i.signature.map_or(0, |t| u64::from(t) + 1));
            put_uv(out, i.enclosing.map_or(0, |t| u64::from(t) + 1));
            put_uv(out, i.relationships.len() as u64);
            for &(t, f) in &i.relationships {
                put_uv(out, u64::from(t));
                put_uv(out, u64::from(f));
            }
        }
    }
    put_uv(out, s.implementations.len() as u64);
    for &i in &s.implementations {
        put_uv(out, u64::from(i));
    }
    put_uv(out, s.postings.len() as u64);
    let mut prev = 0u32;
    for &(d, f) in &s.postings {
        put_uv(out, u64::from(d - prev));
        put_uv(out, u64::from(f));
        prev = d;
    }
}

// ---------------------------------------------------------------------------------------------
// Reader

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

/// A section of the file bytes, for the FSTs.
#[derive(Clone)]
struct Bytes {
    data: Arc<Data>,
    range: StdRange<usize>,
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.data[self.range.clone()]
    }
}

/// A read-only code graph file.
pub struct CodeGraph {
    data: Arc<Data>,
    sections: [StdRange<usize>; NUM_SECTIONS],
    paths: fst::Map<Bytes>,
    symbols: fst::Map<Bytes>,
    num_documents: u32,
    num_symbols: u32,
}

impl std::fmt::Debug for CodeGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeGraph")
            .field("size", &self.data.len())
            .field("documents", &self.num_documents)
            .field("symbols", &self.num_symbols)
            .finish()
    }
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap_or_default()))
        .ok_or_else(|| corrupt("offset past the end"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap_or_default()))
        .ok_or_else(|| corrupt("offset past the end"))
}

/// Entry `i` of a table section.
fn table_get(t: &[u8], i: usize) -> Result<&[u8]> {
    let n = usize::try_from(u64_at(t, 0)?).map_err(|_| corrupt("table size"))?;
    if i >= n {
        return Err(corrupt(format!("table entry {i} of {n}")));
    }
    let data = 8 * (n + 2);
    let a = usize::try_from(u64_at(t, 8 + 8 * i)?).map_err(|_| corrupt("table offset"))?;
    let b = usize::try_from(u64_at(t, 16 + 8 * i)?).map_err(|_| corrupt("table offset"))?;
    t.get(data + a..data + b)
        .filter(|_| a <= b)
        .ok_or_else(|| corrupt("table entry out of bounds"))
}

fn table_len(t: &[u8]) -> Result<u32> {
    u32::try_from(u64_at(t, 0)?).map_err(|_| corrupt("table size"))
}

impl CodeGraph {
    /// Maps and validates the file at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        // SAFETY: code graph files are written once to a temporary file and renamed into
        // place, and never modified afterwards. Every read is bounds-checked.
        #[allow(unsafe_code)]
        let map = unsafe { memmap2::Mmap::map(&file)? };
        Self::parse(Data::Mmap(map))
    }

    /// Reads a code graph held in memory.
    pub fn from_vec(bytes: Vec<u8>) -> Result<Self> {
        Self::parse(Data::Owned(bytes))
    }

    /// Reads a code graph from `r` into memory.
    pub fn read(mut r: impl Read) -> Result<Self> {
        let mut bytes = Vec::new();
        r.read_to_end(&mut bytes)?;
        Self::from_vec(bytes)
    }

    fn parse(data: Data) -> Result<Self> {
        let len = data.len();
        if len < HEADER_LEN as usize + FOOTER_LEN || &data[..8] != MAGIC {
            return Err(corrupt("not a code graph file"));
        }
        let version = u32_at(&data, 8)?;
        if version != VERSION {
            return Err(corrupt(format!("unsupported version {version}")));
        }
        if &data[len - 8..] != END_MAGIC {
            return Err(corrupt("missing footer"));
        }
        let footer = &data[len - FOOTER_LEN..len - 12];
        if crc32fast::hash(footer) != u32_at(&data, len - 12)? {
            return Err(corrupt("footer checksum mismatch"));
        }
        let mut sections: [StdRange<usize>; NUM_SECTIONS] = Default::default();
        for (i, s) in sections.iter_mut().enumerate() {
            let off = usize::try_from(u64_at(footer, 16 * i)?).map_err(|_| corrupt("offset"))?;
            let n = usize::try_from(u64_at(footer, 16 * i + 8)?).map_err(|_| corrupt("length"))?;
            let end = off
                .checked_add(n)
                .ok_or_else(|| corrupt("section bounds"))?;
            if off < HEADER_LEN as usize || end > len - FOOTER_LEN {
                return Err(corrupt(format!("section {i} out of bounds")));
            }
            *s = off..end;
        }
        let data = Arc::new(data);
        let fst_at = |i: usize| -> Result<fst::Map<Bytes>> {
            fst::Map::new(Bytes {
                data: data.clone(),
                range: sections[i].clone(),
            })
            .map_err(|e| corrupt(format!("section {i}: {e}")))
        };
        let paths = fst_at(PATHS_FST)?;
        let symbols = fst_at(SYMBOLS_FST)?;
        let num_documents = table_len(&data[sections[DOC_PATHS].clone()])?;
        let num_symbols = table_len(&data[sections[SYM_NAMES].clone()])?;
        let starts = sections[DOC_STARTS].len();
        if starts != 4 * (num_documents as usize + 1) || sections[DOC_CHUNKS].len() % 12 != 0 {
            return Err(corrupt("document index size"));
        }
        Ok(Self {
            data,
            sections,
            paths,
            symbols,
            num_documents,
            num_symbols,
        })
    }

    fn section(&self, i: usize) -> &[u8] {
        &self.data[self.sections[i].clone()]
    }

    /// The file's size in bytes.
    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn num_documents(&self) -> u32 {
        self.num_documents
    }

    pub fn num_symbols(&self) -> u32 {
        self.num_symbols
    }

    /// The document id of `path`.
    pub fn document_id(&self, path: &str) -> Option<u32> {
        self.paths.get(path).and_then(|v| u32::try_from(v).ok())
    }

    /// The path of document `id`.
    pub fn document_path(&self, id: u32) -> Result<&str> {
        std::str::from_utf8(table_get(self.section(DOC_PATHS), id as usize)?)
            .map_err(|_| corrupt("invalid UTF-8 path"))
    }

    /// All paths with their document ids, sorted by path.
    pub fn paths(&self) -> Vec<(String, u32)> {
        use fst::Streamer;
        let mut out = Vec::with_capacity(self.num_documents as usize);
        let mut s = self.paths.stream();
        while let Some((k, v)) = s.next() {
            out.push((String::from_utf8_lossy(k).into_owned(), v as u32));
        }
        out
    }

    /// The paths under `prefix` (a directory with a trailing `/`, or `""`), sorted.
    pub fn paths_with_prefix(&self, prefix: &str) -> Vec<(String, u32)> {
        use fst::{Automaton, IntoStreamer, Streamer};
        let mut out = Vec::new();
        let mut s = self
            .paths
            .search(fst::automaton::Str::new(prefix).starts_with())
            .into_stream();
        while let Some((k, v)) = s.next() {
            out.push((String::from_utf8_lossy(k).into_owned(), v as u32));
        }
        out
    }

    /// The document at `path`.
    pub fn document(&self, path: &str) -> Result<Option<Document>> {
        match self.document_id(path) {
            Some(id) => self.document_by_id(id).map(Some),
            None => Ok(None),
        }
    }

    /// Document `id`.
    pub fn document_by_id(&self, id: u32) -> Result<Document> {
        if id >= self.num_documents {
            return Err(corrupt(format!("document {id} of {}", self.num_documents)));
        }
        let starts = self.section(DOC_STARTS);
        let a = u32_at(starts, 4 * id as usize)? as usize;
        let b = u32_at(starts, 4 * id as usize + 4)? as usize;
        let chunks = self.section(DOC_CHUNKS);
        let mut doc = Document {
            path: self.document_path(id)?.to_owned(),
            ..Default::default()
        };
        for c in a..b {
            let off = usize::try_from(u64_at(chunks, 12 * c)?).map_err(|_| corrupt("chunk"))?;
            let len = u32_at(chunks, 12 * c + 8)? as usize;
            let docs = self.sections[DOCS].clone();
            if off < docs.start || off + len > docs.end {
                return Err(corrupt("document chunk out of bounds"));
            }
            let raw = lz4_flex::decompress_size_prepended(&self.data[off..off + len])
                .map_err(|e| corrupt(format!("document {id}: {e}")))?;
            self.decode_chunk(&raw, &mut doc, c == a)?;
        }
        if b - a > 1 {
            doc.occurrences.sort_by_key(|o| (o.range, o.symbol));
        }
        Ok(doc)
    }

    fn decode_chunk(&self, raw: &[u8], doc: &mut Document, first: bool) -> Result<()> {
        let mut c = Cursor { buf: raw };
        let encoding = c.u32()?;
        let language = c.str()?;
        if first {
            doc.position_encoding = encoding;
            doc.language = language.to_owned();
        }
        let n = c.count()?;
        doc.occurrences.reserve(n);
        let (mut line, mut ch) = (0u32, 0u32);
        for _ in 0..n {
            let dl = c.u32()?;
            let start_line = line
                .checked_add(dl)
                .ok_or_else(|| corrupt("line overflow"))?;
            let sc = c.u32()?;
            let start_character = if dl == 0 {
                ch.checked_add(sc)
                    .ok_or_else(|| corrupt("column overflow"))?
            } else {
                sc
            };
            (line, ch) = (start_line, start_character);
            let end_line = start_line
                .checked_add(c.u32()?)
                .ok_or_else(|| corrupt("line overflow"))?;
            let end_character = c.u32()?;
            let symbol = match c.u32()? {
                0 => None,
                s => Some(s - 1),
            };
            let roles = c.u32()?;
            let syntax_kind = c.u32()?;
            let flags = c.uv()?;
            let mut occ = Occurrence {
                range: Range {
                    start_line,
                    start_character,
                    end_line,
                    end_character,
                },
                symbol,
                roles,
                syntax_kind,
                ..Default::default()
            };
            if flags & 1 != 0 {
                for _ in 0..c.count()? {
                    occ.override_documentation
                        .push(self.text(c.u32()?)?.to_owned());
                }
            }
            if flags & 2 != 0 {
                for _ in 0..c.count()? {
                    let severity = c.u32()?;
                    let code = c.str()?.to_owned();
                    let message = c.str()?.to_owned();
                    let source = c.str()?.to_owned();
                    let tags = (0..c.count()?)
                        .map(|_| c.u32())
                        .collect::<Result<Vec<_>>>()?;
                    occ.diagnostics.push(Diagnostic {
                        severity,
                        code,
                        message,
                        source,
                        tags,
                    });
                }
            }
            if flags & 4 != 0 {
                occ.enclosing_range = Some(Range {
                    start_line: c.u32()?,
                    start_character: c.u32()?,
                    end_line: c.u32()?,
                    end_character: c.u32()?,
                });
            }
            doc.occurrences.push(occ);
        }
        Ok(())
    }

    /// The occurrences of `path` containing the position, innermost first.
    pub fn occurrences_at(&self, path: &str, line: u32, character: u32) -> Result<Vec<Occurrence>> {
        let Some(doc) = self.document(path)? else {
            return Ok(Vec::new());
        };
        let mut hits: Vec<Occurrence> = doc
            .occurrences
            .into_iter()
            .take_while(|o| o.range.start_line <= line)
            .filter(|o| o.range.contains(line, character))
            .collect();
        hits.sort_by_key(|o| {
            let r = o.range;
            (
                std::cmp::Reverse((r.start_line, r.start_character)),
                (r.end_line, r.end_character),
            )
        });
        Ok(hits)
    }

    /// Hover text `id`.
    pub fn text(&self, id: u32) -> Result<&str> {
        std::str::from_utf8(table_get(self.section(TEXTS), id as usize)?)
            .map_err(|_| corrupt("invalid UTF-8 text"))
    }

    /// The id of a symbol.
    pub fn symbol_id(&self, symbol: &str) -> Option<u32> {
        self.symbols.get(symbol).and_then(|v| u32::try_from(v).ok())
    }

    /// The string of symbol `id`.
    pub fn symbol_name(&self, id: u32) -> Result<&str> {
        std::str::from_utf8(table_get(self.section(SYM_NAMES), id as usize)?)
            .map_err(|_| corrupt("invalid UTF-8 symbol"))
    }

    /// Looks a symbol up by its string.
    pub fn symbol(&self, symbol: &str) -> Result<Option<Symbol>> {
        match self.symbol_id(symbol) {
            Some(id) => self.symbol_by_id(id).map(Some),
            None => Ok(None),
        }
    }

    /// Symbol `id`.
    pub fn symbol_by_id(&self, id: u32) -> Result<Symbol> {
        let mut c = Cursor {
            buf: table_get(self.section(SYM_RECORDS), id as usize)?,
        };
        let info = if c.uv()? != 0 {
            let kind = c.u32()?;
            let display_name = c.str()?.to_owned();
            let documentation = (0..c.count()?)
                .map(|_| Ok(self.text(c.u32()?)?.to_owned()))
                .collect::<Result<Vec<_>>>()?;
            let signature = match c.u32()? {
                0 => None,
                t => Some(self.text(t - 1)?.to_owned()),
            };
            let enclosing_symbol = match c.u32()? {
                0 => None,
                s => Some(s - 1),
            };
            let relationships = (0..c.count()?)
                .map(|_| {
                    let symbol = c.u32()?;
                    let flags = u8::try_from(c.uv()?).map_err(|_| corrupt("flags"))?;
                    Ok(Relationship { symbol, flags })
                })
                .collect::<Result<Vec<_>>>()?;
            Some(SymbolInfo {
                kind,
                display_name,
                documentation,
                signature,
                enclosing_symbol,
                relationships,
            })
        } else {
            None
        };
        let implementations = (0..c.count()?)
            .map(|_| c.u32())
            .collect::<Result<Vec<_>>>()?;
        let mut postings = Vec::new();
        let mut doc = 0u32;
        for _ in 0..c.count()? {
            doc = doc
                .checked_add(c.u32()?)
                .ok_or_else(|| corrupt("posting overflow"))?;
            let flags = u8::try_from(c.uv()?).map_err(|_| corrupt("flags"))?;
            postings.push(Posting {
                document: doc,
                flags,
            });
        }
        Ok(Symbol {
            id,
            name: self.symbol_name(id)?.to_owned(),
            info,
            implementations,
            postings,
        })
    }

    /// The postings of symbol `id`, read without decoding the rest of its record (the
    /// ranking export reads every symbol's).
    pub fn symbol_postings(&self, id: u32) -> Result<Vec<Posting>> {
        let mut c = Cursor {
            buf: table_get(self.section(SYM_RECORDS), id as usize)?,
        };
        if c.uv()? != 0 {
            c.u32()?; // kind
            c.str()?; // display name
            for _ in 0..c.count()? {
                c.u32()?; // documentation
            }
            c.u32()?; // signature
            c.u32()?; // enclosing symbol
            for _ in 0..c.count()? {
                c.u32()?;
                c.uv()?;
            }
        }
        for _ in 0..c.count()? {
            c.u32()?; // implementations
        }
        let mut postings = Vec::new();
        let mut doc = 0u32;
        for _ in 0..c.count()? {
            doc = doc
                .checked_add(c.u32()?)
                .ok_or_else(|| corrupt("posting overflow"))?;
            let flags = u8::try_from(c.uv()?).map_err(|_| corrupt("flags"))?;
            postings.push(Posting {
                document: doc,
                flags,
            });
        }
        Ok(postings)
    }
}
