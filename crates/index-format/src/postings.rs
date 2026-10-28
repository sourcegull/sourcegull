//! Document-level postings.
//!
//! Each list starts with a tag byte and a varint count.
//! - Tag 0, blocks: the first doc id as a varint, then the gaps minus one
//!   in blocks of 128, each a bit width byte and the gaps bitpacked
//!   (little-endian bit order). Runs of consecutive ids pack to zero bits.
//! - Tag 1: a serialized Roaring bitmap, used when it is smaller (long,
//!   dense lists).

use roaring::RoaringBitmap;

use crate::{Result, corrupt, varint};

const BLOCKS: u8 = 0;
const ROARING: u8 = 1;
const BLOCK: usize = 128;

/// Bits of the FST value holding the doc count; the rest is the offset.
pub(crate) const COUNT_BITS: u32 = 20;
pub(crate) const COUNT_MAX: u64 = (1 << COUNT_BITS) - 1;

pub(crate) fn fst_value(offset: u64, count: usize) -> u64 {
    (offset << COUNT_BITS) | (count as u64).min(COUNT_MAX)
}

pub(crate) fn split_fst_value(v: u64) -> (u64, u64) {
    (v >> COUNT_BITS, v & COUNT_MAX)
}

/// Appends the encoding of the sorted, distinct `docs`.
pub(crate) fn encode(out: &mut Vec<u8>, docs: &[u32]) {
    let start = out.len();
    out.push(BLOCKS);
    varint::put(out, docs.len() as u64);
    let Some((&first, _)) = docs.split_first() else {
        return;
    };
    varint::put(out, u64::from(first));
    let gaps: Vec<u32> = docs.windows(2).map(|w| w[1] - w[0] - 1).collect();
    for block in gaps.chunks(BLOCK) {
        let width = block.iter().fold(0, |m, &g| m | g);
        let width = (32 - width.leading_zeros()) as usize;
        out.push(width as u8);
        let mut acc = 0u64;
        let mut bits = 0usize;
        for &g in block {
            acc |= u64::from(g) << bits;
            bits += width;
            while bits >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                bits -= 8;
            }
        }
        if bits > 0 {
            out.push(acc as u8);
        }
    }
    // A bitmap only pays off for long, dense lists.
    if docs.len() >= 4096 {
        let bm = RoaringBitmap::from_sorted_iter(docs.iter().copied()).expect("sorted docs");
        if bm.serialized_size() + 6 < out.len() - start {
            out.truncate(start);
            out.push(ROARING);
            varint::put(out, docs.len() as u64);
            bm.serialize_into(&mut *out).expect("write to a Vec");
        }
    }
}

/// The doc count of the list at `pos`.
pub(crate) fn count(data: &[u8], mut pos: usize) -> Result<u64> {
    pos += 1;
    varint::get(data, &mut pos)
}

/// Decodes the list at `pos`; every id must be below `max_doc`.
pub(crate) fn decode(data: &[u8], mut pos: usize, max_doc: u32) -> Result<RoaringBitmap> {
    let tag = *data.get(pos).ok_or_else(|| corrupt("postings offset"))?;
    pos += 1;
    let n = varint::get_len(data, &mut pos, max_doc as usize)?;
    match tag {
        BLOCKS => {
            if n == 0 {
                return Ok(RoaringBitmap::new());
            }
            let mut ids = Vec::with_capacity(n);
            let out_of_range = || corrupt("posting doc id out of range");
            let mut cur = varint::get(data, &mut pos)?;
            if cur >= u64::from(max_doc) {
                return Err(out_of_range());
            }
            ids.push(cur);
            let mut left = n - 1;
            while left > 0 {
                let len = left.min(BLOCK);
                let width = usize::from(*data.get(pos).ok_or_else(|| corrupt("postings block"))?);
                pos += 1;
                if width > 32 {
                    return Err(corrupt("postings bit width"));
                }
                let bytes = (len * width).div_ceil(8);
                let packed = data
                    .get(pos..pos + bytes)
                    .ok_or_else(|| corrupt("postings block truncated"))?;
                pos += bytes;
                let mask = (1u64 << width) - 1;
                let mut acc = 0u64;
                let mut bits = 0usize;
                let mut next = packed.iter();
                for _ in 0..len {
                    while bits < width {
                        acc |= u64::from(*next.next().expect("sized above")) << bits;
                        bits += 8;
                    }
                    // Ids stay below `max_doc` (a u32), so the sum cannot overflow.
                    cur += (acc & mask) + 1;
                    if cur >= u64::from(max_doc) {
                        return Err(out_of_range());
                    }
                    acc >>= width;
                    bits -= width;
                    ids.push(cur);
                }
                left -= len;
            }
            // Gaps are at least 1 and every id is below `max_doc`, so the ids are strictly
            // increasing u32s.
            Ok(
                RoaringBitmap::from_sorted_iter(ids.into_iter().map(|d| d as u32))
                    .expect("strictly increasing"),
            )
        }
        ROARING => {
            let rest = data
                .get(pos..)
                .ok_or_else(|| corrupt("postings bitmap offset"))?;
            let bm = RoaringBitmap::deserialize_from(rest)
                .map_err(|e| corrupt(format!("postings bitmap: {e}")))?;
            if bm.len() != n as u64 || bm.max().is_some_and(|m| m >= max_doc) {
                return Err(corrupt("postings bitmap does not match its count"));
            }
            Ok(bm)
        }
        _ => Err(corrupt("postings tag")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_and_dense_round_trip() {
        for docs in [
            vec![],
            vec![0],
            vec![3, 9, 1000, 70000],
            vec![u32::MAX - 2, u32::MAX - 1],
            (0..300).map(|i| i * i).collect(),
            (0..5000).collect::<Vec<u32>>(),
            (0..5000).step_by(3).collect(),
            (0..30000).collect(),
        ] {
            let mut buf = vec![0xAA];
            encode(&mut buf, &docs);
            assert_eq!(count(&buf, 1).unwrap(), docs.len() as u64);
            let bm = decode(&buf, 1, u32::MAX).unwrap();
            assert_eq!(bm.iter().collect::<Vec<_>>(), docs);
        }
        // Consecutive ids pack to zero bits per gap.
        let mut run = Vec::new();
        encode(&mut run, &(0..20000).collect::<Vec<u32>>());
        assert_eq!(run[0], BLOCKS);
        assert!(run.len() < 200, "{}", run.len());
        // Bitmap containers (1 bit per id) beat 2-3 bit gaps.
        let mut dense = Vec::new();
        encode(
            &mut dense,
            &(0..60000).filter(|i| i % 7 < 4).collect::<Vec<u32>>(),
        );
        assert_eq!(dense[0], ROARING);
        let mut sparse = Vec::new();
        encode(
            &mut sparse,
            &(0..5000).map(|i| i * 13).collect::<Vec<u32>>(),
        );
        assert_eq!(sparse[0], BLOCKS);
        assert!(sparse.len() < 5000 * 4 / 8 + 100);
        let mut buf = Vec::new();
        encode(&mut buf, &[5, 10]);
        assert!(decode(&buf, 0, 10).is_err());
    }

    #[test]
    fn corrupt_lists_are_errors() {
        // A first id beyond any u32, then a gap that would overflow it.
        let mut huge = vec![BLOCKS];
        crate::varint::put(&mut huge, 2);
        crate::varint::put(&mut huge, u64::MAX);
        huge.extend([32, 0xff, 0xff, 0xff, 0xff]);
        assert!(decode(&huge, 0, u32::MAX).is_err());
        // A bitmap offset past the end.
        assert!(decode(&[ROARING, 1], 0, 10).is_err());
    }
}
