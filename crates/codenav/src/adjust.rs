//! Position adjustment between commits: when an upload's commit is not the viewed
//! one, lines map through the file's diff. Lines in changed parts of a hunk have no
//! counterpart; columns pass through unchanged on the others.

/// One hunk, with 0-based starts.
#[derive(Debug, Clone)]
struct Hunk {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
    /// Per body line: b' ', b'-' or b'+'.
    ops: Vec<u8>,
}

/// Maps lines of a file between two commits (`old` → `new` and back).
#[derive(Debug, Clone, Default)]
pub struct LineMap {
    hunks: Vec<Hunk>,
    /// The file does not exist on the other side, or its diff is unknown (binary, too
    /// large): no line maps.
    absent: bool,
}

impl LineMap {
    /// The identity (the file is unchanged).
    pub fn identity() -> Self {
        Self::default()
    }

    /// No line maps.
    pub fn absent() -> Self {
        Self {
            hunks: Vec::new(),
            absent: true,
        }
    }

    /// From unified-diff hunks: (old start, old lines, new start, new lines, body) with git's
    /// 1-based starts (the line before, for an empty side).
    pub fn from_hunks<'a>(hunks: impl IntoIterator<Item = (u32, u32, u32, u32, &'a str)>) -> Self {
        let start0 = |start: u32, len: u32| if len == 0 { start } else { start - 1 };
        let mut out: Vec<Hunk> = hunks
            .into_iter()
            .map(|(os, ol, ns, nl, body)| Hunk {
                old_start: start0(os, ol),
                old_len: ol,
                new_start: start0(ns, nl),
                new_len: nl,
                ops: body
                    .lines()
                    .filter_map(|l| l.bytes().next())
                    .filter(|b| matches!(b, b' ' | b'-' | b'+'))
                    .collect(),
            })
            .collect();
        out.sort_by_key(|h| h.old_start);
        Self {
            hunks: out,
            absent: false,
        }
    }

    /// The new line of old `line`, if it is unchanged.
    pub fn old_to_new(&self, line: u32) -> Option<u32> {
        self.map(line, false)
    }

    /// The old line of new `line`, if it is unchanged.
    pub fn new_to_old(&self, line: u32) -> Option<u32> {
        self.map(line, true)
    }

    fn map(&self, line: u32, reverse: bool) -> Option<u32> {
        if self.absent {
            return None;
        }
        let mut delta: i64 = 0;
        for h in &self.hunks {
            let (from_start, from_len, to_start, to_len, from_op, to_op) = if reverse {
                (h.new_start, h.new_len, h.old_start, h.old_len, b'+', b'-')
            } else {
                (h.old_start, h.old_len, h.new_start, h.new_len, b'-', b'+')
            };
            if line < from_start {
                break;
            }
            if line >= from_start + from_len {
                delta = i64::from(to_start + to_len) - i64::from(from_start + from_len);
                continue;
            }
            let (mut from, mut to) = (from_start, to_start);
            for &op in &h.ops {
                if op == b' ' {
                    if from == line {
                        return Some(to);
                    }
                    from += 1;
                    to += 1;
                } else if op == from_op {
                    if from == line {
                        return None;
                    }
                    from += 1;
                } else if op == to_op {
                    to += 1;
                }
            }
            // A body shorter than its header says.
            return None;
        }
        u32::try_from(i64::from(line) + delta).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::LineMap;

    #[test]
    fn identity_and_absent() {
        assert_eq!(LineMap::identity().old_to_new(7), Some(7));
        assert_eq!(LineMap::absent().old_to_new(0), None);
        assert_eq!(LineMap::absent().new_to_old(0), None);
    }

    #[test]
    fn maps_through_hunks() {
        // old:  a b c d e f g h      new:  a b X c d f g Y Z h
        // Hunk 1 inserts X after b; hunk 2 drops e; hunk 3 replaces nothing but adds Y Z.
        let map = LineMap::from_hunks([
            (1, 3, 1, 4, " a\n b\n+X\n c\n"),
            (4, 3, 5, 2, " d\n-e\n f\n"),
            (7, 2, 7, 4, " g\n+Y\n+Z\n h\n"),
        ]);
        let old: Vec<Option<u32>> = (0..9).map(|l| map.old_to_new(l)).collect();
        assert_eq!(
            old,
            [
                Some(0),
                Some(1),
                Some(3),
                Some(4),
                None,
                Some(5),
                Some(6),
                Some(9),
                Some(10)
            ]
        );
        let new: Vec<Option<u32>> = (0..10).map(|l| map.new_to_old(l)).collect();
        assert_eq!(
            new,
            [
                Some(0),
                Some(1),
                None,
                Some(2),
                Some(3),
                Some(5),
                Some(6),
                None,
                None,
                Some(7)
            ]
        );
    }

    #[test]
    fn lines_outside_hunks_shift() {
        // Two lines inserted at the top of a long file, with three lines of context.
        let map = LineMap::from_hunks([(1, 3, 1, 5, "+x\n+y\n a\n b\n c\n")]);
        assert_eq!(map.old_to_new(0), Some(2));
        assert_eq!(map.old_to_new(100), Some(102));
        assert_eq!(map.new_to_old(0), None);
        assert_eq!(map.new_to_old(102), Some(100));
    }

    #[test]
    fn empty_old_side() {
        // A file that was empty: git writes `@@ -0,0 +1,2 @@`.
        let map = LineMap::from_hunks([(0, 0, 1, 2, "+a\n+b\n")]);
        assert_eq!(map.new_to_old(0), None);
        assert_eq!(map.new_to_old(1), None);
    }
}
