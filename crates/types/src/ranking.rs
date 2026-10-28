//! Static file ranks from the code graph: what the ranking job stores per
//! repository and the indexer writes into shards.
//!
//! A rank is in `[0, 1]`: 0 for files nothing references (or that no SCIP upload covers), 1 for
//! the best-ranked file of the instance. Ranks are stored quantized to 16 bits, as a compact
//! payload per repository ([`encode_ranks`]):
//!
//! ```text
//! u8 version (1), varint count,
//! per path, sorted: varint bytes shared with the previous path, varint suffix length,
//!                   suffix bytes, u16 rank (little-endian)
//! ```

use std::collections::HashMap;

const PAYLOAD_VERSION: u8 = 1;

/// The ranks of one repository's files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathRanks {
    /// The ranking run that last changed these ranks (positive); shards record it, so a
    /// changed generation means the repository's shards are rebuilt.
    pub generation: i64,
    /// Quantized ([`quantize`]).
    ranks: HashMap<String, u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid path ranks payload: {0}")]
pub struct RanksDecodeError(&'static str);

impl PathRanks {
    pub fn new(generation: i64, ranks: impl IntoIterator<Item = (String, f32)>) -> Self {
        Self {
            generation,
            ranks: ranks.into_iter().map(|(p, r)| (p, quantize(r))).collect(),
        }
    }

    /// Decodes a payload written by [`encode_ranks`].
    pub fn decode(generation: i64, payload: &[u8]) -> Result<Self, RanksDecodeError> {
        Ok(Self::new(generation, decode_ranks(payload)?))
    }

    /// The rank of `path`; 0 when it has none.
    pub fn get(&self, path: &str) -> f32 {
        self.ranks.get(path).copied().map_or(0.0, dequantize)
    }

    pub fn len(&self) -> usize {
        self.ranks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranks.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, f32)> {
        self.ranks.iter().map(|(p, r)| (p.as_str(), dequantize(*r)))
    }
}

/// A rank in `[0, 1]` as 16 bits.
pub fn quantize(rank: f32) -> u16 {
    let r = if rank.is_nan() {
        0.0
    } else {
        rank.clamp(0.0, 1.0)
    };
    (r * f32::from(u16::MAX)).round() as u16
}

/// The rank a quantized value stands for.
pub fn dequantize(q: u16) -> f32 {
    f32::from(q) / f32::from(u16::MAX)
}

fn put_uv(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_uv(buf: &[u8], pos: &mut usize) -> Result<u64, RanksDecodeError> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = *buf.get(*pos).ok_or(RanksDecodeError("truncated varint"))?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(RanksDecodeError("overlong varint"))
}

/// Encodes ranks (in any order; a path given twice keeps its first rank) as a payload.
pub fn encode_ranks(ranks: &[(String, f32)]) -> Vec<u8> {
    let mut order: Vec<&(String, f32)> = ranks.iter().collect();
    order.sort_by(|a, b| a.0.cmp(&b.0));
    order.dedup_by(|b, a| a.0 == b.0);
    let mut out = Vec::with_capacity(1 + order.len() * 12);
    out.push(PAYLOAD_VERSION);
    put_uv(&mut out, order.len() as u64);
    let mut prev: &[u8] = &[];
    for (path, rank) in order {
        let p = path.as_bytes();
        let shared = prev.iter().zip(p).take_while(|(a, b)| a == b).count();
        put_uv(&mut out, shared as u64);
        put_uv(&mut out, (p.len() - shared) as u64);
        out.extend_from_slice(&p[shared..]);
        out.extend_from_slice(&quantize(*rank).to_le_bytes());
        prev = p;
    }
    out
}

/// Decodes a payload into (path, rank) pairs in path order.
pub fn decode_ranks(payload: &[u8]) -> Result<Vec<(String, f32)>, RanksDecodeError> {
    let (&version, _) = payload.split_first().ok_or(RanksDecodeError("empty"))?;
    if version != PAYLOAD_VERSION {
        return Err(RanksDecodeError("unknown version"));
    }
    let mut pos = 1;
    let n = get_uv(payload, &mut pos)?;
    // Each entry takes at least four bytes.
    if n > (payload.len() as u64) / 4 {
        return Err(RanksDecodeError("count past the end"));
    }
    let mut out: Vec<(String, f32)> = Vec::with_capacity(n as usize);
    let mut prev: Vec<u8> = Vec::new();
    for _ in 0..n {
        let shared = get_uv(payload, &mut pos)? as usize;
        let len = get_uv(payload, &mut pos)? as usize;
        if shared > prev.len() {
            return Err(RanksDecodeError("shared prefix too long"));
        }
        let end = pos
            .checked_add(len)
            .filter(|&e| e + 2 <= payload.len())
            .ok_or(RanksDecodeError("entry past the end"))?;
        prev.truncate(shared);
        prev.extend_from_slice(&payload[pos..end]);
        let q = u16::from_le_bytes([payload[end], payload[end + 1]]);
        pos = end + 2;
        let path = String::from_utf8(prev.clone()).map_err(|_| RanksDecodeError("bad UTF-8"))?;
        out.push((path, dequantize(q)));
    }
    if pos != payload.len() {
        return Err(RanksDecodeError("trailing bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip() {
        let ranks = vec![
            ("src/main.rs".to_owned(), 0.25),
            ("README.md".to_owned(), 0.0),
            ("src/lib.rs".to_owned(), 1.0),
            ("src/lib/inner.rs".to_owned(), 0.5),
            ("src/lib.rs".to_owned(), 0.7),
        ];
        let payload = encode_ranks(&ranks);
        let decoded = decode_ranks(&payload).unwrap();
        let paths: Vec<&str> = decoded.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["README.md", "src/lib.rs", "src/lib/inner.rs", "src/main.rs"]
        );
        let r = PathRanks::decode(7, &payload).unwrap();
        assert_eq!(r.generation, 7);
        assert_eq!(r.get("src/lib.rs"), 1.0);
        assert!((r.get("src/main.rs") - 0.25).abs() < 1e-4);
        assert_eq!(r.get("missing"), 0.0);
        assert_eq!(r.len(), 4);
        assert_eq!(decode_ranks(&encode_ranks(&[])).unwrap(), vec![]);
        assert!(decode_ranks(&payload[..payload.len() - 1]).is_err());
        assert!(decode_ranks(&[]).is_err());
        assert_eq!(quantize(f32::NAN), 0);
        assert_eq!(quantize(2.0), u16::MAX);
    }
}
