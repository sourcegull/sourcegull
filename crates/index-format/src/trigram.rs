//! Case-folded byte trigrams.
//!
//! Content is folded by lowercasing ASCII letters only; other bytes are kept,
//! so a case-insensitive query looks up each non-ASCII case variant itself.
//! A trigram is three folded bytes packed into the low 24 bits of a `u32`.

use std::cell::RefCell;

/// ASCII-lowercases one byte.
#[inline]
pub fn fold(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// The trigram of three bytes, folded.
#[inline]
pub fn trigram_bytes(t: [u8; 3]) -> u32 {
    (u32::from(fold(t[0])) << 16) | (u32::from(fold(t[1])) << 8) | u32::from(fold(t[2]))
}

thread_local! {
    /// One bit per possible trigram (2 MiB), all clear between calls.
    static SEEN: RefCell<Vec<u64>> = RefCell::new(vec![0; 1 << 18]);
}

/// The sorted, distinct trigrams of `content`. Duplicates are dropped with
/// a per-thread bitset while scanning, so only distinct trigrams are sorted.
pub fn trigrams(content: &[u8]) -> Vec<u32> {
    let mut out = distinct(content);
    out.sort_unstable();
    out
}

/// The distinct trigrams of `content` in order of first occurrence. The
/// writer needs no order (a doc's id is appended to each trigram's list),
/// and sorting would take about as long as the scan.
pub(crate) fn distinct(content: &[u8]) -> Vec<u32> {
    if content.len() < 3 {
        return Vec::new();
    }
    SEEN.with_borrow_mut(|seen| {
        let mut out = Vec::new();
        let mut t = (u32::from(fold(content[0])) << 8) | u32::from(fold(content[1]));
        for &b in &content[2..] {
            t = ((t << 8) | u32::from(fold(b))) & 0xff_ffff;
            let (word, bit) = ((t >> 6) as usize, 1u64 << (t & 63));
            if seen[word] & bit == 0 {
                seen[word] |= bit;
                out.push(t);
            }
        }
        for &t in &out {
            seen[(t >> 6) as usize] = 0;
        }
        out
    })
}

pub(crate) fn key(t: u32) -> [u8; 3] {
    [(t >> 16) as u8, (t >> 8) as u8, t as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_ascii_only() {
        let t = trigrams("AbCabc".as_bytes());
        assert_eq!(
            t,
            [
                trigram_bytes(*b"abc"),
                trigram_bytes(*b"bca"),
                trigram_bytes(*b"cab")
            ]
        );
        assert_eq!(trigram_bytes(*b"ABC"), trigram_bytes(*b"abc"));
        let e = "É".as_bytes();
        assert_ne!(
            trigrams(&[e[0], e[1], b'x']),
            trigrams(&["é".as_bytes()[0], "é".as_bytes()[1], b'x'])
        );
        assert!(trigrams(b"ab").is_empty());
        // The seen-set is clear again for the next call.
        assert_eq!(trigrams(b"abcabc"), trigrams(b"abcabc"));
        let mut naive: Vec<u32> = b"the quick brown fox the quick"
            .windows(3)
            .map(|w| trigram_bytes([w[0], w[1], w[2]]))
            .collect();
        naive.sort_unstable();
        naive.dedup();
        assert_eq!(trigrams(b"the quick brown fox the quick"), naive);
        assert_eq!(key(trigram_bytes(*b"xyz")), *b"xyz");
        let mut unsorted = distinct(b"the quick brown fox the quick");
        unsorted.sort_unstable();
        assert_eq!(unsorted, naive);
    }
}
