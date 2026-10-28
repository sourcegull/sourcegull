//! The PATHS section: front-coded strings in blocks.
//!
//! ```text
//! count u32, block count u32, block offsets u32 × blocks (from data start), data
//! block: varint len, bytes; then per string varint shared, varint suffix len, suffix
//! ```

use crate::{Result, corrupt, varint};

pub(crate) const BLOCK: usize = 16;

#[derive(Default)]
pub(crate) struct PathsBuilder {
    count: u32,
    offsets: Vec<u32>,
    data: Vec<u8>,
    prev: Vec<u8>,
}

impl PathsBuilder {
    pub(crate) fn len(&self) -> u32 {
        self.count
    }

    pub(crate) fn last(&self) -> Option<&[u8]> {
        (self.count > 0).then_some(self.prev.as_slice())
    }

    pub(crate) fn push(&mut self, path: &[u8]) {
        if self.count as usize % BLOCK == 0 {
            self.offsets.push(self.data.len() as u32);
            varint::put(&mut self.data, path.len() as u64);
            self.data.extend_from_slice(path);
        } else {
            let shared = self
                .prev
                .iter()
                .zip(path)
                .take_while(|(a, b)| a == b)
                .count();
            varint::put(&mut self.data, shared as u64);
            varint::put(&mut self.data, (path.len() - shared) as u64);
            self.data.extend_from_slice(&path[shared..]);
        }
        self.prev.clear();
        self.prev.extend_from_slice(path);
        self.count += 1;
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.offsets.len() * 4 + self.data.len());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&(self.offsets.len() as u32).to_le_bytes());
        for o in &self.offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&self.data);
        out
    }
}

/// A parsed view of the PATHS section.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Paths<'a> {
    pub(crate) count: u32,
    offsets: &'a [u8],
    data: &'a [u8],
}

impl<'a> Paths<'a> {
    pub(crate) fn parse(s: &'a [u8]) -> Result<Paths<'a>> {
        let u32_at = |i: usize| -> Result<u32> {
            s.get(i..i + 4)
                .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
                .ok_or_else(|| corrupt("paths header truncated"))
        };
        let count = u32_at(0)?;
        let blocks = u32_at(4)? as usize;
        if blocks != (count as usize).div_ceil(BLOCK) {
            return Err(corrupt("paths block count"));
        }
        let data_start = blocks
            .checked_mul(4)
            .and_then(|n| n.checked_add(8))
            .filter(|&n| n <= s.len())
            .ok_or_else(|| corrupt("paths block table truncated"))?;
        Ok(Paths {
            count,
            offsets: &s[8..data_start],
            data: &s[data_start..],
        })
    }

    fn block_start(&self, block: usize) -> Result<usize> {
        let o = u32::from_le_bytes(
            self.offsets[block * 4..block * 4 + 4]
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        if o > self.data.len() {
            return Err(corrupt("paths block offset"));
        }
        Ok(o)
    }

    /// Decodes strings from index `from` on, calling `f` with each index and string.
    pub(crate) fn walk(
        &self,
        from: u32,
        mut f: impl FnMut(u32, &[u8]) -> std::ops::ControlFlow<()>,
    ) -> Result<()> {
        if from >= self.count {
            return Ok(());
        }
        let block = from as usize / BLOCK;
        let mut pos = self.block_start(block)?;
        let mut cur: Vec<u8> = Vec::new();
        let mut i = (block * BLOCK) as u32;
        while i < self.count {
            if i as usize % BLOCK == 0 {
                pos = self.block_start(i as usize / BLOCK)?;
                let len = varint::get_len(self.data, &mut pos, self.data.len())?;
                cur.clear();
                cur.extend_from_slice(self.take(&mut pos, len)?);
            } else {
                let shared = varint::get_len(self.data, &mut pos, cur.len())?;
                let len = varint::get_len(self.data, &mut pos, self.data.len())?;
                cur.truncate(shared);
                cur.extend_from_slice(self.take(&mut pos, len)?);
            }
            if i >= from && f(i, &cur).is_break() {
                break;
            }
            i += 1;
        }
        Ok(())
    }

    fn take(&self, pos: &mut usize, len: usize) -> Result<&'a [u8]> {
        let s = self
            .data
            .get(*pos..*pos + len)
            .ok_or_else(|| corrupt("path past the end"))?;
        *pos += len;
        Ok(s)
    }

    pub(crate) fn get(&self, i: u32) -> Result<Vec<u8>> {
        let mut out = None;
        self.walk(i, |_, p| {
            out = Some(p.to_vec());
            std::ops::ControlFlow::Break(())
        })?;
        out.ok_or_else(|| corrupt("path index out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::ControlFlow;

    #[test]
    fn round_trip_across_blocks() {
        let paths: Vec<String> = (0..50)
            .map(|i| format!("src/dir{}/f{i}.rs", i / 7))
            .collect();
        let mut b = PathsBuilder::default();
        for p in &paths {
            b.push(p.as_bytes());
        }
        let bytes = b.finish();
        let t = Paths::parse(&bytes).unwrap();
        assert_eq!(t.count, 50);
        let mut all = Vec::new();
        t.walk(0, |_, p| {
            all.push(String::from_utf8(p.to_vec()).unwrap());
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(all, paths);
        for i in [0, 15, 16, 17, 33, 49] {
            assert_eq!(t.get(i).unwrap(), paths[i as usize].as_bytes());
        }
        assert!(t.get(50).is_err());
    }
}
