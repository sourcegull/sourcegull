//! Streaming SCIP decoding: walks the top-level `Index` message field by field, so
//! only one document is in memory at a time however large the index is.
//!
//! An `Index` is a sequence of (tag, length, bytes) records for `metadata` (1), `documents`
//! (2) and `external_symbols` (3). Each record is read into a reused buffer and decoded on its
//! own. Unknown fields are skipped. Gzip input (what `src code-intel upload` sends) is detected
//! by its magic bytes and decompressed on the fly.

use std::io::{self, BufRead, BufReader, Read};

use prost::Message;

use crate::proto::{
    Document, INDEX_DOCUMENTS, INDEX_EXTERNAL_SYMBOLS, INDEX_METADATA, Metadata, SymbolInformation,
};

/// The largest single record (one document) accepted.
pub const MAX_RECORD: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("reading the index: {0}")]
    Io(#[from] io::Error),
    #[error("the index is truncated")]
    Truncated,
    #[error("the index is not a SCIP protobuf: {0}")]
    Malformed(String),
}

/// One top-level item of an index.
#[derive(Debug)]
pub enum Item {
    Metadata(Metadata),
    Document(Document),
    ExternalSymbol(SymbolInformation),
}

/// Iterates the items of a SCIP index.
pub struct IndexReader<R> {
    input: R,
    buf: Vec<u8>,
    /// Bytes of protobuf read so far (after decompression), for error messages.
    offset: u64,
}

impl IndexReader<Box<dyn BufRead + Send>> {
    /// Reads `input`, decompressing it when it starts with the gzip magic.
    pub fn open(input: impl Read + Send + 'static) -> io::Result<Self> {
        let mut input = BufReader::with_capacity(1 << 16, input);
        let gzip = input.fill_buf()?.starts_with(&[0x1f, 0x8b]);
        let input: Box<dyn BufRead + Send> = if gzip {
            Box::new(BufReader::with_capacity(
                1 << 16,
                flate2::bufread::MultiGzDecoder::new(input),
            ))
        } else {
            Box::new(input)
        };
        Ok(Self::new(input))
    }
}

impl<R: BufRead> IndexReader<R> {
    /// Reads an uncompressed index.
    pub fn new(input: R) -> Self {
        Self {
            input,
            buf: Vec::new(),
            offset: 0,
        }
    }

    /// The next item, or `None` at the end of the index.
    pub fn next_item(&mut self) -> Result<Option<Item>, DecodeError> {
        loop {
            let Some(key) = self.varint(true)? else {
                return Ok(None);
            };
            let field = u32::try_from(key >> 3)
                .map_err(|_| self.malformed(format!("field number {}", key >> 3)))?;
            let wire = key & 7;
            match wire {
                0 => {
                    self.varint(false)?;
                }
                1 => self.skip(8)?,
                5 => self.skip(4)?,
                2 => {
                    let len = self.varint(false)?.unwrap_or_default();
                    if len > MAX_RECORD {
                        return Err(self.malformed(format!("a {len}-byte record")));
                    }
                    if !matches!(
                        field,
                        INDEX_METADATA | INDEX_DOCUMENTS | INDEX_EXTERNAL_SYMBOLS
                    ) {
                        self.skip(len)?;
                        continue;
                    }
                    self.read_record(len)?;
                    let at = self.offset;
                    let bad = |e: prost::DecodeError| {
                        DecodeError::Malformed(format!("field {field} before byte {at}: {e}"))
                    };
                    let buf = self.buf.as_slice();
                    return Ok(Some(match field {
                        INDEX_METADATA => Item::Metadata(Metadata::decode(buf).map_err(bad)?),
                        INDEX_DOCUMENTS => Item::Document(Document::decode(buf).map_err(bad)?),
                        _ => Item::ExternalSymbol(SymbolInformation::decode(buf).map_err(bad)?),
                    }));
                }
                w => return Err(self.malformed(format!("wire type {w} for field {field}"))),
            }
        }
    }

    fn malformed(&self, what: String) -> DecodeError {
        DecodeError::Malformed(format!("{what} at byte {}", self.offset))
    }

    /// Reads a varint; `None` on a clean end of input when `eof_ok`.
    fn varint(&mut self, eof_ok: bool) -> Result<Option<u64>, DecodeError> {
        let mut value = 0u64;
        for i in 0..10 {
            let byte = match self.input.fill_buf()?.first() {
                Some(&b) => b,
                None if i == 0 && eof_ok => return Ok(None),
                None => return Err(DecodeError::Truncated),
            };
            self.input.consume(1);
            self.offset += 1;
            value |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(Some(value));
            }
        }
        Err(self.malformed("an overlong varint".into()))
    }

    fn read_record(&mut self, len: u64) -> Result<(), DecodeError> {
        let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
        self.buf.clear();
        // Grow as data arrives rather than trusting the length up front.
        let n = (&mut self.input)
            .take(len as u64)
            .read_to_end(&mut self.buf)?;
        self.offset += n as u64;
        if n < len {
            return Err(DecodeError::Truncated);
        }
        // Don't keep a huge buffer around after one large document.
        if self.buf.capacity() > 64 << 20 && len < 1 << 20 {
            self.buf.shrink_to(1 << 20);
        }
        Ok(())
    }

    fn skip(&mut self, len: u64) -> Result<(), DecodeError> {
        let n = io::copy(&mut (&mut self.input).take(len), &mut io::sink())?;
        self.offset += n;
        if n < len {
            return Err(DecodeError::Truncated);
        }
        Ok(())
    }
}
