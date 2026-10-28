//! The SYMBOLS section: per-doc symbol lists.
//!
//! ```text
//! doc count u32, extractor version u32 (0: no symbols extracted),
//! data offsets u32 × (doc count + 1) (from data start), data length u32,
//! data, strings
//! data per doc:    per symbol: kind u8, name varint, container varint
//!                  (0: none, else string offset + 1), start varint,
//!                  length varint (the name's byte range in the content)
//! strings:         varint length + UTF-8, deduplicated, referred to by offset
//! ```
//!
//! About 10 bytes per symbol plus distinct names. Offsets are checked when
//! the shard opens; a doc's data when it is read, so a bad record is an
//! error, never a panic.

use std::ops::Range;

use rustc_hash::FxHashMap;

use crate::{DocSymbol, Result, corrupt, varint};

/// Where the data offsets start.
const OFFSETS: usize = 8;

#[derive(Default)]
pub(crate) struct SymbolsBuilder {
    pub(crate) version: u32,
    offsets: Vec<u32>,
    data: Vec<u8>,
    strings: Vec<u8>,
    string_offs: FxHashMap<String, u64>,
}

impl SymbolsBuilder {
    fn string(&mut self, s: String) -> u64 {
        if let Some(&off) = self.string_offs.get(&s) {
            return off;
        }
        let off = self.strings.len() as u64;
        varint::put(&mut self.strings, s.len() as u64);
        self.strings.extend_from_slice(s.as_bytes());
        self.string_offs.insert(s, off);
        off
    }

    /// Adds the next doc's symbols; those outside its `size` bytes are left
    /// out.
    pub(crate) fn push_doc(&mut self, symbols: Vec<DocSymbol>, size: u32) {
        self.offsets.push(self.data.len() as u32);
        for sym in symbols {
            if sym.range.start > sym.range.end || sym.range.end > size {
                continue;
            }
            let name = self.string(sym.name);
            let container = sym.container.map_or(0, |c| self.string(c) + 1);
            self.data.push(sym.kind);
            varint::put(&mut self.data, name);
            varint::put(&mut self.data, container);
            varint::put(&mut self.data, u64::from(sym.range.start));
            varint::put(&mut self.data, u64::from(sym.range.end - sym.range.start));
        }
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let docs = self.offsets.len() as u32;
        self.offsets.push(self.data.len() as u32);
        let mut out =
            Vec::with_capacity(12 + self.offsets.len() * 4 + self.data.len() + self.strings.len());
        out.extend_from_slice(&docs.to_le_bytes());
        out.extend_from_slice(&self.version.to_le_bytes());
        for o in &self.offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.data);
        out.extend_from_slice(&self.strings);
        out
    }

    /// Whether the data still fits the u32 offsets.
    pub(crate) fn fits(&self) -> bool {
        self.data.len() < u32::MAX as usize
    }
}

/// Where the parts of the section start.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SymbolsLayout {
    pub(crate) version: u32,
    data: usize,
    strings: usize,
}

impl SymbolsLayout {
    /// Checks the header and the offsets for `docs` docs.
    pub(crate) fn parse(s: &[u8], docs: u32) -> Result<SymbolsLayout> {
        let u32_at = |o: usize| -> Result<u32> {
            let b = s
                .get(o..o + 4)
                .ok_or_else(|| corrupt("symbols truncated"))?;
            Ok(u32::from_le_bytes(b.try_into().expect("4 bytes")))
        };
        if u32_at(0)? != docs {
            return Err(corrupt("symbols: doc count"));
        }
        let version = u32_at(4)?;
        let len_at = OFFSETS + (docs as usize + 1) * 4;
        let data = len_at + 4;
        let strings = data
            .checked_add(u32_at(len_at)? as usize)
            .filter(|&e| e <= s.len())
            .ok_or_else(|| corrupt("symbols truncated"))?;
        let mut prev = 0;
        for i in 0..=docs as usize {
            let o = u32_at(OFFSETS + i * 4)?;
            if o < prev || data + o as usize > strings {
                return Err(corrupt(format!("symbols of doc {i} out of order")));
            }
            prev = o;
        }
        if data + prev as usize != strings {
            return Err(corrupt("symbols: data length"));
        }
        Ok(SymbolsLayout {
            version,
            data,
            strings,
        })
    }

    fn doc_data<'s>(&self, s: &'s [u8], doc: u32) -> &'s [u8] {
        let at = |i: usize| {
            let o = OFFSETS + i * 4;
            u32::from_le_bytes(s[o..o + 4].try_into().expect("4 bytes")) as usize
        };
        &s[self.data + at(doc as usize)..self.data + at(doc as usize + 1)]
    }

    /// The symbols of doc `doc`, whose content is `size` bytes.
    pub(crate) fn read(&self, s: &[u8], doc: u32, size: u32) -> Result<Vec<DocSymbol>> {
        let data = self.doc_data(s, doc);
        let strings = &s[self.strings..];
        let string = |off: u64| -> Result<String> {
            let mut pos = usize::try_from(off).map_err(|_| corrupt("symbol name offset"))?;
            let len = varint::get(strings, &mut pos)? as usize;
            let bytes = strings
                .get(pos..pos.saturating_add(len))
                .ok_or_else(|| corrupt("symbol name out of range"))?;
            String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("symbol name is not UTF-8"))
        };
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let kind = data[pos];
            pos += 1;
            let name = string(varint::get(data, &mut pos)?)?;
            let container = match varint::get(data, &mut pos)? {
                0 => None,
                c => Some(string(c - 1)?),
            };
            let start = varint::get(data, &mut pos)?;
            let end = varint::get(data, &mut pos)?
                .checked_add(start)
                .filter(|&e| e <= u64::from(size))
                .ok_or_else(|| corrupt(format!("doc {doc}: symbol out of range")))?;
            out.push(DocSymbol {
                name,
                container,
                kind,
                range: start as u32..end as u32,
            });
        }
        Ok(out)
    }

    /// The kinds and name ranges of a doc's symbols, without their names
    /// (for ranking, which only needs where definitions are).
    pub(crate) fn read_ranges(
        &self,
        s: &[u8],
        doc: u32,
        size: u32,
    ) -> Result<Vec<(u8, Range<u32>)>> {
        let data = self.doc_data(s, doc);
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let kind = data[pos];
            pos += 1;
            varint::get(data, &mut pos)?;
            varint::get(data, &mut pos)?;
            let start = varint::get(data, &mut pos)?;
            let end = varint::get(data, &mut pos)?
                .checked_add(start)
                .filter(|&e| e <= u64::from(size))
                .ok_or_else(|| corrupt(format!("doc {doc}: symbol out of range")))?;
            out.push((kind, start as u32..end as u32));
        }
        Ok(out)
    }

    /// Whether the doc has symbols.
    pub(crate) fn has_symbols(&self, s: &[u8], doc: u32) -> bool {
        !self.doc_data(s, doc).is_empty()
    }
}
