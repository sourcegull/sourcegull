//! Chunk matches: match ranges grouped into line chunks, the shape of the
//! streaming API's `chunkMatches`.

use std::ops::Range;

/// A position in a file. `line` and `column` are 0-based; `column` counts
/// UTF-8 bytes within the line, as `offset` counts bytes in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchRange {
    pub start: Location,
    pub end: Location,
}

/// Whole lines of the file holding one or more nearby matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMatch {
    pub content: String,
    pub content_start: Location,
    pub ranges: Vec<MatchRange>,
}

/// A symbol whose name matched a symbol search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    pub name: String,
    pub container: Option<String>,
    /// LSP symbol kind number.
    pub kind: u8,
    /// Where the name is in the file.
    pub range: MatchRange,
}

/// The location of byte `offset` in a file whose newlines are at the sorted
/// byte offsets `newlines`.
pub fn locate(newlines: &[u32], offset: usize) -> Location {
    let line = newlines.partition_point(|&n| (n as usize) < offset);
    let line_start = match line {
        0 => 0,
        l => newlines[l - 1] as usize + 1,
    };
    Location {
        offset,
        line,
        column: offset - line_start,
    }
}

/// Groups sorted, non-overlapping `ranges` into chunks; matches whose lines
/// touch or overlap share a chunk. Lines are cut at `max_line_len` bytes (on a
/// char boundary) when set; ranges keep their true positions.
pub fn chunk_matches(
    content: &[u8],
    ranges: &[Range<usize>],
    max_line_len: Option<usize>,
) -> Vec<ChunkMatch> {
    let mut chunks: Vec<ChunkMatch> = Vec::new();
    // (chunk start, chunk end exclusive of the final newline, last line)
    let mut current: Option<(Location, usize, usize, Vec<MatchRange>)> = None;
    let mut cursor = LineCursor::default();
    for r in ranges {
        let start = cursor.locate(content, r.start);
        let end = cursor.locate(content, r.end);
        let line_start = Location {
            offset: r.start - start.column,
            line: start.line,
            column: 0,
        };
        let line_end = line_end(content, r.end.max(r.start));
        let range = MatchRange { start, end };
        match &mut current {
            Some((_, cend, cline, rs)) if line_start.line <= *cline + 1 => {
                *cend = (*cend).max(line_end);
                *cline = (*cline).max(cursor.line_of(content, line_end));
                rs.push(range);
            }
            _ => {
                if let Some(c) = current.take() {
                    chunks.push(finish(content, c, max_line_len));
                }
                let last = cursor.line_of(content, line_end);
                current = Some((line_start, line_end, last, vec![range]));
            }
        }
    }
    if let Some(c) = current {
        chunks.push(finish(content, c, max_line_len));
    }
    chunks
}

fn finish(
    content: &[u8],
    (start, end, _, ranges): (Location, usize, usize, Vec<MatchRange>),
    max_line_len: Option<usize>,
) -> ChunkMatch {
    let text = String::from_utf8_lossy(&content[start.offset..end]);
    let text = match max_line_len {
        None => text.into_owned(),
        Some(max) => text
            .split('\n')
            .map(|l| truncate(l, max))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    ChunkMatch {
        content: text,
        content_start: start,
        ranges,
    }
}

fn truncate(line: &str, max: usize) -> &str {
    if line.len() <= max {
        return line;
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

fn line_end(content: &[u8], from: usize) -> usize {
    // A range ending just after a newline ends on the previous line.
    let from = if from > 0 && content.get(from - 1) == Some(&b'\n') {
        from - 1
    } else {
        from
    };
    content[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(content.len(), |p| from + p)
}

/// Converts offsets to line/column; offsets must be visited in increasing order.
#[derive(Default)]
struct LineCursor {
    offset: usize,
    line: usize,
    line_start: usize,
}

impl LineCursor {
    fn advance(&mut self, content: &[u8], to: usize) {
        if to < self.offset {
            *self = LineCursor::default();
        }
        for (i, &b) in content[self.offset..to].iter().enumerate() {
            if b == b'\n' {
                self.line += 1;
                self.line_start = self.offset + i + 1;
            }
        }
        self.offset = to;
    }

    fn locate(&mut self, content: &[u8], offset: usize) -> Location {
        self.advance(content, offset);
        Location {
            offset,
            line: self.line,
            column: offset - self.line_start,
        }
    }

    fn line_of(&self, content: &[u8], offset: usize) -> usize {
        if offset < self.offset {
            return self.line;
        }
        self.line
            + content[self.offset..offset]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn groups_adjacent_lines() {
        let text = b"a foo\nc foo\n\n\nfoo d\n";
        let chunks = chunk_matches(text, &[2..5, 8..11, 14..17], None);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].content, "a foo\nc foo");
        assert_eq!(chunks[0].ranges.len(), 2);
        let c = &chunks[0];
        assert_eq!(
            c.ranges[0].start,
            Location {
                offset: 2,
                line: 0,
                column: 2
            }
        );
        assert_eq!(chunks[1].content, "foo d");
        assert_eq!(chunks[1].content_start.line, 4);
    }

    #[test]
    fn locates_offsets_by_newlines() {
        let newlines = [3, 4, 9];
        let at = |o| {
            let l = locate(&newlines, o);
            (l.line, l.column)
        };
        assert_eq!(
            [at(0), at(3), at(4), at(5), at(9), at(12)],
            [(0, 0), (0, 3), (1, 0), (2, 0), (2, 4), (3, 2)]
        );
    }

    #[test]
    fn multiline_match_and_truncation() {
        let text = "x ab\ncd y\nzz".as_bytes();
        let chunks = chunk_matches(text, &[2..7], Some(3));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "x a\ncd ");
        assert_eq!(
            chunks[0].ranges[0].end,
            Location {
                offset: 7,
                line: 1,
                column: 2
            }
        );
    }
}
