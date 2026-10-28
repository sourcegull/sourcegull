//! The per-file skip table.
//!
//! One pass over a file pairs its delimiters and finds its strings and
//! comments, so the matcher can step over a balanced group or a whole
//! string in O(1) and never needs to backtrack across delimiters.
//! Delimiters inside strings and comments do not count. A closing
//! delimiter without its opener is a wall holes cannot cross; an opener
//! without its closer is an ordinary character.

use crate::syntax::Syntax;

/// What starts at a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Plain,
    /// An opening delimiter with its closer at `end - 1`.
    Open,
    /// A closing delimiter.
    Close,
    /// A string spanning to `end`.
    Opaque,
    /// A comment spanning to `end`.
    Comment,
    /// Inside a string or comment.
    Inside,
}

pub(crate) struct Scan {
    kind: Vec<Kind>,
    /// For `Open`, `Opaque` and `Comment` bytes, the exclusive end of the unit,
    /// sorted by start.
    ends: Vec<(u32, u32)>,
}

pub(crate) fn is_open(b: u8) -> Option<u8> {
    match b {
        b'(' => Some(b')'),
        b'[' => Some(b']'),
        b'{' => Some(b'}'),
        _ => None,
    }
}

pub(crate) fn is_close(b: u8) -> bool {
    matches!(b, b')' | b']' | b'}')
}

impl Scan {
    pub(crate) fn new(src: &[u8], syntax: &Syntax) -> Scan {
        let n = src.len();
        let mut kind = vec![Kind::Plain; n];
        let mut ends: Vec<(u32, u32)> = Vec::new();
        let mut stack: Vec<(usize, u8)> = Vec::new();
        // Bytes that can start a string or comment; others skip the checks.
        let mut starts = [false; 256];
        let firsts = syntax
            .line_comments
            .iter()
            .chain(syntax.block_comments.iter().map(|(o, _)| o))
            .chain(syntax.strings.iter().map(|s| &s.open))
            .filter_map(|t| t.as_bytes().first());
        for &b in firsts {
            starts[usize::from(b)] = true;
        }
        if syntax.char_literals {
            starts[usize::from(b'\'')] = true;
        }
        let mut i = 0;
        while i < n {
            if starts[usize::from(src[i])]
                && let Some((e, k)) = opaque_at(src, i, syntax)
            {
                kind[i] = k;
                ends.push((i as u32, e as u32));
                for k in &mut kind[i + 1..e] {
                    *k = Kind::Inside;
                }
                i = e;
                continue;
            }
            let b = src[i];
            if let Some(close) = is_open(b) {
                stack.push((i, close));
            } else if is_close(b) {
                kind[i] = Kind::Close;
                // Pair with the nearest opener of this kind; openers above
                // it stay unpaired.
                if let Some(at) = stack.iter().rposition(|&(_, c)| c == b) {
                    let (open, _) = stack[at];
                    stack.truncate(at);
                    kind[open] = Kind::Open;
                    ends.push((open as u32, (i + 1) as u32));
                }
            }
            i += 1;
        }
        ends.sort_unstable();
        Scan { kind, ends }
    }

    pub(crate) fn kind(&self, i: usize) -> Kind {
        self.kind[i]
    }

    /// The exclusive end of the group or string starting at `i`.
    pub(crate) fn end(&self, i: usize) -> usize {
        let at = self.ends.partition_point(|&(s, _)| (s as usize) < i);
        debug_assert_eq!(self.ends.get(at).map(|e| e.0 as usize), Some(i));
        self.ends.get(at).map_or(i + 1, |&(_, e)| e as usize)
    }

    /// The end of the comment starting at `i`, if one does.
    pub(crate) fn comment_end(&self, i: usize) -> Option<usize> {
        (self.kind[i] == Kind::Comment).then(|| self.end(i))
    }

    /// The end of the unit starting at `i` (a character, a balanced group,
    /// a string or a comment), or `None` at a closing delimiter.
    pub(crate) fn unit_end(&self, src: &[u8], i: usize) -> Option<usize> {
        match self.kind[i] {
            Kind::Open | Kind::Opaque | Kind::Comment => Some(self.end(i)),
            Kind::Close => None,
            Kind::Plain | Kind::Inside => Some(i + char_len(src[i])),
        }
    }
}

/// The byte length of the UTF-8 character starting with `b` (1 for a
/// stray continuation byte).
pub(crate) fn char_len(b: u8) -> usize {
    match b {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// The end and kind of the string or comment starting at `i`, if one does.
fn opaque_at(src: &[u8], i: usize, syntax: &Syntax) -> Option<(usize, Kind)> {
    let rest = &src[i..];
    for c in syntax.line_comments {
        if rest.starts_with(c.as_bytes()) {
            let end = i + memchr::memchr(b'\n', rest).unwrap_or(rest.len());
            return Some((end, Kind::Comment));
        }
    }
    for (open, close) in syntax.block_comments {
        if rest.starts_with(open.as_bytes()) {
            let end = block_end(src, i, open, close, syntax.nested_comments);
            return Some((end, Kind::Comment));
        }
    }
    for s in syntax.strings {
        if !rest.starts_with(s.open.as_bytes()) {
            continue;
        }
        let mut j = i + s.open.len();
        while j < src.len() {
            if s.escapes && src[j] == b'\\' {
                j += 2;
                continue;
            }
            if src[j..].starts_with(s.close.as_bytes()) {
                return Some((j + s.close.len(), Kind::Opaque));
            }
            if !s.multiline && src[j] == b'\n' {
                break;
            }
            j += 1;
        }
        // Unterminated: not a string.
        return None;
    }
    if syntax.char_literals && src[i] == b'\'' {
        return char_literal(src, i).map(|e| (e, Kind::Opaque));
    }
    None
}

/// The end of a block comment opened at `i` (the end of the file when it
/// is not closed).
fn block_end(src: &[u8], i: usize, open: &str, close: &str, nested: bool) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j < src.len() {
        if src[j..].starts_with(open.as_bytes()) && (nested || depth == 0) {
            depth += 1;
            j += open.len();
        } else if src[j..].starts_with(close.as_bytes()) {
            depth -= 1;
            j += close.len();
            if depth == 0 {
                return j;
            }
        } else {
            j += 1;
        }
    }
    src.len()
}

/// `'x'` or `'\…'` (up to 10 bytes) starting at `i`.
fn char_literal(src: &[u8], i: usize) -> Option<usize> {
    let next = *src.get(i + 1)?;
    if next == b'\\' {
        let window = &src[i + 2..src.len().min(i + 12)];
        return memchr::memchr(b'\'', window).map(|k| i + 2 + k + 1);
    }
    if next == b'\'' || next == b'\n' {
        return None;
    }
    let after = i + 1 + char_len(next);
    (src.get(after) == Some(&b'\'')).then_some(after + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::{GENERIC, syntax_for};

    #[test]
    fn pairs_delimiters_outside_strings_and_comments() {
        let src = b"f(a, \")\", [b]) // )\n}";
        let scan = Scan::new(src, syntax_for("go"));
        assert_eq!(scan.kind(1), Kind::Open);
        assert_eq!(scan.end(1), 14);
        assert_eq!(scan.kind(5), Kind::Opaque);
        assert_eq!(scan.end(5), 8);
        assert_eq!(scan.kind(6), Kind::Inside);
        assert_eq!(scan.kind(10), Kind::Open);
        assert_eq!(scan.kind(15), Kind::Comment);
        assert_eq!(scan.kind(src.len() - 1), Kind::Close);
        assert_eq!(scan.unit_end(src, src.len() - 1), None);
    }

    #[test]
    fn unbalanced_and_char_literals() {
        let src = b"(a [b) ']' 'ab";
        let scan = Scan::new(src, syntax_for("rust"));
        assert_eq!(scan.kind(0), Kind::Open);
        assert_eq!(scan.end(0), 6);
        // `[` lost its pair: an ordinary character.
        assert_eq!(scan.kind(3), Kind::Plain);
        assert_eq!(scan.kind(7), Kind::Opaque);
        assert_eq!(scan.end(7), 10);
        // A lifetime-like quote is not a literal.
        assert_eq!(scan.kind(11), Kind::Plain);
        // Generic: no comments; unterminated strings are not strings.
        let scan = Scan::new(b"a \"b\nc\" // (x)", &GENERIC);
        assert_eq!(scan.kind(2), Kind::Plain);
        assert_eq!(scan.kind(11), Kind::Open);
    }

    #[test]
    fn nested_block_comments() {
        let src = b"/* a /* b */ c */x";
        assert_eq!(Scan::new(src, syntax_for("rust")).end(0), 17);
        assert_eq!(Scan::new(src, syntax_for("c")).end(0), 12);
    }
}
