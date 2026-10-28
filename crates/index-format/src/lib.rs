//! sourcegull-index-format: the index shard file format, v1.
//!
//! A shard holds one repository's indexed branches, or, as a compound
//! shard, those of many small repositories; docs are sorted by
//! repository, so each repository's docs are a contiguous id range
//! ([`Shard::repo_docs`]). All integers are little-endian. The file starts
//! with a fixed header and a section table (TOC), each section with its own
//! CRC-32. Sections follow the header back to back, so every byte is
//! covered by a checksum:
//!
//! ```text
//! header   magic "GULLSHRD", version u32, flags u32 (content codec),
//!          section count u32, reserved u32,
//!          TOC entries {kind u32, 0 u32, offset u64, len u64, crc u32, 0 u32},
//!          header crc u32
//! REPOS    repo_id i32, rank u16, branch count u16, name,
//!          branches {name, commit [u8; 20]}; strings are u32 length + UTF-8
//! LANGS    language ids referenced by docs (lang 0 = none, n = LANGS[n-1])
//! DOCS     fixed 72-byte rows ([`Doc`]), sorted by (repo, path)
//! PATHS    front-coded path strings in doc order, 16 per block
//! CONTENT  per-doc stored content (none or LZ4 block)
//! NEWLINES per-doc newline offsets, delta varints
//! TRIGRAMS FST: case-folded trigram (3 bytes) → postings offset and count
//! POSTINGS per trigram, the sorted doc ids holding it: delta varints, or a
//!          Roaring bitmap when that is smaller
//! SYMBOLS  per doc, the symbols defined in it: kind, name, container and
//!          the name's byte range, varint-coded, with deduplicated name
//!          strings (layout in `symbols.rs`)
//! TOMBSTONES (optional, delta shards only) the base shard's id u32, then a
//!          Roaring bitmap of the base doc ids this delta replaces
//! RANKS    (optional, since version 3) repo count u32, per repo the
//!          generation of its ranks u64 (0: none), then per doc its static
//!          rank f32 in [0, 1] (file-level PageRank)
//! ```
//!
//! A shard without RANKS (written before version 3, or without ranks) has
//! rank 0 for every doc and generation 0 for every repository. A delta
//! shard copies the base's generation and gives each path the rank it has
//! in the base, so ranks change only with full builds.
//!
//! A delta shard holds every current version of each path that
//! changed since its base was built, on any branch, with the same branch
//! list as the base. Base docs of those paths are tombstoned; the other
//! base docs keep their branch masks. A shard's id is its header checksum
//! ([`Shard::id`]), so a delta names exactly the base it applies to.
//!
//! Postings are document-level, not positional. Docs are shared
//! across branches through `branch_mask`. Binary and too-large files are
//! kept as docs (path search lists them) with no content or trigrams.
//!
//! [`ShardWriter`] builds a shard from [`PreparedDoc`]s, which
//! [`prepare_doc`] makes independently per file so builds can run it in
//! parallel. [`Shard`] reads a shard through mmap and checks every
//! checksum and reference when it opens, so a corrupt file is an error,
//! never a panic.

mod paths;
mod postings;
mod reader;
mod symbols;
mod trigram;
mod varint;
mod writer;

pub use reader::{Doc, PathIter, Shard};
use std::ops::Range;
pub use trigram::{fold, trigram_bytes, trigrams};
pub use writer::{Codec, DocInput, PreparedDoc, SYNC_MIN, ShardWriter, prepare_copy, prepare_doc};

/// File magic.
pub const MAGIC: &[u8; 8] = b"GULLSHRD";
/// Format version written. Version 2 added SYMBOLS (older shards fail to
/// open and are rebuilt); version 3 added the optional RANKS section.
pub const VERSION: u32 = 3;
/// The oldest format version read.
pub const MIN_VERSION: u32 = 2;
/// File name extension for shards.
pub const EXTENSION: &str = "gullshard";

/// Doc flag: binary content (a NUL byte in the first 8 KB); not searchable by content.
pub const FLAG_BINARY: u16 = 1;
/// Doc flag: larger than the size limit; not searchable by content.
pub const FLAG_TOO_LARGE: u16 = 2;

/// A repository in a shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMeta {
    pub id: i32,
    pub name: String,
    pub rank: u16,
    /// Bit `i` of a doc's `branch_mask` is `branches[i]`.
    pub branches: Vec<BranchMeta>,
}

/// A symbol defined in a doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSymbol {
    pub name: String,
    /// The enclosing definition's name.
    pub container: Option<String>,
    /// LSP symbol kind number.
    pub kind: u8,
    /// Byte range of the name in the doc's content.
    pub range: Range<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchMeta {
    pub name: String,
    pub commit: [u8; 20],
}

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("shard I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt shard: {0}")]
    Corrupt(String),
    #[error("unsupported shard version {0}")]
    Version(u32),
    #[error("invalid shard input: {0}")]
    Invalid(String),
}

pub type Result<T, E = FormatError> = std::result::Result<T, E>;

pub(crate) fn corrupt(msg: impl Into<String>) -> FormatError {
    FormatError::Corrupt(msg.into())
}

/// Section kinds in the TOC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Section {
    Repos = 1,
    Langs = 2,
    Docs = 3,
    Paths = 4,
    Content = 5,
    Newlines = 6,
    Trigrams = 7,
    Postings = 8,
    Tombstones = 9,
    Symbols = 10,
    Ranks = 11,
}

/// Required sections; [`Section::Tombstones`] and [`Section::Ranks`] are
/// optional.
pub(crate) const SECTIONS: [Section; 9] = [
    Section::Repos,
    Section::Langs,
    Section::Docs,
    Section::Paths,
    Section::Content,
    Section::Newlines,
    Section::Trigrams,
    Section::Postings,
    Section::Symbols,
];

impl Section {
    /// The section's index in [`SECTIONS`]; panics for optional sections.
    pub(crate) fn slot(self) -> usize {
        SECTIONS
            .iter()
            .position(|s| *s == self)
            .expect("a required section")
    }
}

pub(crate) const HEADER_FIXED: usize = 24;
pub(crate) const TOC_ENTRY: usize = 32;
pub(crate) const DOC_ROW: usize = 72;
