//! Reading the indexer's name and version from a SCIP index (`Index.metadata.tool_info`),
//! without decoding the documents: top-level fields other than `metadata` are skipped.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

/// The `metadata.tool_info` of an index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
}

/// Whether `path` starts with the gzip magic bytes.
pub fn is_gzip(path: &Path) -> io::Result<bool> {
    let mut magic = [0u8; 2];
    let mut f = std::fs::File::open(path)?;
    let mut n = 0;
    while n < 2 {
        match f.read(&mut magic[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n == 2 && magic == [0x1f, 0x8b])
}

/// The tool info of the index at `path` (gzip-compressed or not).
pub fn read_tool_info(path: &Path) -> io::Result<ToolInfo> {
    let file = std::fs::File::open(path)?;
    let reader: Box<dyn Read> = if is_gzip(path)? {
        Box::new(flate2::read::MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    tool_info(BufReader::new(reader))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// A varint, or `None` at a clean end of input.
fn varint(r: &mut impl BufRead) -> io::Result<Option<u64>> {
    let mut value = 0u64;
    for i in 0..10 {
        let mut b = [0u8; 1];
        if r.read(&mut b)? == 0 {
            return if i == 0 {
                Ok(None)
            } else {
                Err(invalid("truncated varint"))
            };
        }
        value |= u64::from(b[0] & 0x7f) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(Some(value));
        }
    }
    Err(invalid("varint too long"))
}

fn skip(r: &mut impl BufRead, wire_type: u64) -> io::Result<()> {
    let n = match wire_type {
        0 => {
            varint(r)?.ok_or_else(|| invalid("truncated field"))?;
            return Ok(());
        }
        1 => 8,
        2 => varint(r)?.ok_or_else(|| invalid("truncated field"))?,
        5 => 4,
        _ => return Err(invalid("unsupported wire type")),
    };
    let copied = io::copy(&mut r.take(n), &mut io::sink())?;
    if copied != n {
        return Err(invalid("truncated field"));
    }
    Ok(())
}

/// Length-delimited fields of a small message: `(field number, bytes)`.
fn fields(mut buf: &[u8]) -> io::Result<Vec<(u64, &[u8])>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let Some(tag) = varint(&mut buf)? else { break };
        let (field, wire_type) = (tag >> 3, tag & 7);
        if wire_type == 2 {
            let len = varint(&mut buf)?.ok_or_else(|| invalid("truncated field"))?;
            let len = usize::try_from(len).map_err(|_| invalid("field too long"))?;
            if len > buf.len() {
                return Err(invalid("truncated field"));
            }
            out.push((field, &buf[..len]));
            buf = &buf[len..];
        } else {
            skip(&mut buf, wire_type)?;
        }
    }
    Ok(out)
}

/// Reads an `Index` up to its first `metadata` field (1) and returns `tool_info` (field 2 of
/// `Metadata`; `name` 1, `version` 2).
fn tool_info(mut r: impl BufRead) -> io::Result<ToolInfo> {
    const MAX_METADATA: u64 = 64 << 20;
    let missing = || invalid("the SCIP index has no valid metadata (tool name and version)");
    while let Some(tag) = varint(&mut r)? {
        let (field, wire_type) = (tag >> 3, tag & 7);
        if field != 1 || wire_type != 2 {
            skip(&mut r, wire_type)?;
            continue;
        }
        let len = varint(&mut r)?.ok_or_else(|| invalid("truncated metadata"))?;
        if len > MAX_METADATA {
            return Err(invalid("metadata too large"));
        }
        let mut metadata = Vec::new();
        (&mut r).take(len).read_to_end(&mut metadata)?;
        if metadata.len() as u64 != len {
            return Err(invalid("truncated metadata"));
        }
        let Some((_, tool)) = fields(&metadata)?.into_iter().rev().find(|(f, _)| *f == 2) else {
            return Err(missing());
        };
        let mut info = ToolInfo::default();
        for (f, bytes) in fields(tool)? {
            let text = || String::from_utf8_lossy(bytes).into_owned();
            match f {
                1 => info.name = text(),
                2 => info.version = text(),
                _ => {}
            }
        }
        return Ok(info);
    }
    Err(missing())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
        put_varint(out, field << 3 | 2);
        put_varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    /// A SCIP index with one document before the metadata.
    pub(crate) fn index(name: &str, version: &str) -> Vec<u8> {
        let mut tool = Vec::new();
        put_bytes(&mut tool, 1, name.as_bytes());
        put_bytes(&mut tool, 2, version.as_bytes());
        put_bytes(&mut tool, 3, b"--flag");
        let mut metadata = Vec::new();
        put_varint(&mut metadata, 1 << 3); // version: 0
        put_varint(&mut metadata, 0);
        put_bytes(&mut metadata, 2, &tool);
        put_bytes(&mut metadata, 3, b"file:///src");
        let mut out = Vec::new();
        put_bytes(&mut out, 2, &[0u8; 300]); // a document, skipped
        put_bytes(&mut out, 1, &metadata);
        put_bytes(&mut out, 2, b"more");
        out
    }

    #[test]
    fn reads_tool_info_plain_and_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("index.scip");
        std::fs::write(&plain, index("scip-go", "v0.1.10")).unwrap();
        assert!(!is_gzip(&plain).unwrap());
        let want = ToolInfo {
            name: "scip-go".into(),
            version: "v0.1.10".into(),
        };
        assert_eq!(read_tool_info(&plain).unwrap(), want);

        let gz = dir.path().join("index.scip.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut enc, &index("scip-go", "v0.1.10")).unwrap();
        std::fs::write(&gz, enc.finish().unwrap()).unwrap();
        assert!(is_gzip(&gz).unwrap());
        assert_eq!(read_tool_info(&gz).unwrap(), want);

        let empty = dir.path().join("empty.scip");
        std::fs::write(&empty, b"").unwrap();
        let err = read_tool_info(&empty).unwrap_err();
        assert!(err.to_string().contains("valid metadata"), "{err}");
        let truncated = dir.path().join("truncated.scip");
        std::fs::write(&truncated, &index("x", "y")[..320]).unwrap();
        assert!(read_tool_info(&truncated).is_err());
    }
}
