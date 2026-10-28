//! LEB128 varints.

use crate::{Result, corrupt};

pub(crate) fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a varint at `*pos`, advancing it.
pub(crate) fn get(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *data
            .get(*pos)
            .ok_or_else(|| corrupt("varint past the end"))?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b < 0x80 {
            return Ok(v);
        }
    }
    Err(corrupt("varint too long"))
}

/// Reads a varint that must fit a `usize` bounded by `max`.
pub(crate) fn get_len(data: &[u8], pos: &mut usize, max: usize) -> Result<usize> {
    let v = get(data, pos)?;
    usize::try_from(v)
        .ok()
        .filter(|&n| n <= max)
        .ok_or_else(|| corrupt("length out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        let vals = [0, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for v in vals {
            put(&mut buf, v);
        }
        let mut pos = 0;
        for v in vals {
            assert_eq!(get(&buf, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, buf.len());
        assert!(get(&buf, &mut pos).is_err());
        assert!(get(&[0xff; 11], &mut 0).is_err());
    }
}
