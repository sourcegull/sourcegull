//! Occurrences: per-line ranges with UTF-16 columns, the compact packed
//! form sent to the viewer, and the HTML form.

use serde::{Deserialize, Serialize};

use crate::kind::SyntaxKind;

/// A highlighted range on one line. Columns count UTF-16 code units, which is
/// what CodeMirror (and JavaScript strings) use; `end_col` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Occurrence {
    /// 0-based line.
    pub line: u32,
    /// 0-based start column (UTF-16).
    pub start_col: u32,
    /// Exclusive end column (UTF-16).
    pub end_col: u32,
    /// What the range is.
    pub kind: SyntaxKind,
}

/// Converts sorted, non-overlapping byte spans into per-line occurrences,
/// splitting spans at line breaks and merging touching spans of one kind.
pub(crate) fn spans_to_occurrences(
    source: &[u8],
    spans: &[(usize, usize, SyntaxKind)],
) -> Vec<Occurrence> {
    let mut out: Vec<Occurrence> = Vec::with_capacity(spans.len());
    // Cursor over the source: byte position, line, UTF-16 column.
    let (mut pos, mut line, mut col) = (0usize, 0u32, 0u32);
    let advance = |to: usize, pos: &mut usize, line: &mut u32, col: &mut u32| {
        while *pos < to {
            let b = source[*pos];
            if b == b'\n' {
                *line += 1;
                *col = 0;
                *pos += 1;
                continue;
            }
            let len = utf8_len(b);
            *col += if len == 4 { 2 } else { 1 };
            *pos += len.max(1);
        }
    };
    let mut push = |o: Occurrence| {
        if o.start_col == o.end_col {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last.line == o.line && last.end_col == o.start_col && last.kind == o.kind {
                last.end_col = o.end_col;
                return;
            }
        }
        out.push(o);
    };
    for &(start, end, kind) in spans {
        let end = end.min(source.len());
        if start >= end || start < pos {
            continue;
        }
        advance(start, &mut pos, &mut line, &mut col);
        let (mut l, mut c) = (line, col);
        let mut seg_start_col = c;
        let mut p = pos;
        while p < end {
            let b = source[p];
            if b == b'\n' {
                push(Occurrence {
                    line: l,
                    start_col: seg_start_col,
                    end_col: c,
                    kind,
                });
                l += 1;
                c = 0;
                seg_start_col = 0;
                p += 1;
                continue;
            }
            let len = utf8_len(b);
            c += if len == 4 { 2 } else { 1 };
            p += len.max(1);
        }
        push(Occurrence {
            line: l,
            start_col: seg_start_col,
            end_col: c,
            kind,
        });
        pos = p;
        line = l;
        col = c;
    }
    out
}

/// Byte length of a UTF-8 sequence from its first byte (1 for invalid bytes,
/// which count as one replacement character).
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Packs occurrences into a flat `u32` array, four values each, in the style
/// of LSP semantic tokens:
///
/// `[line_delta, start, length, kind]`
///
/// `line_delta` is relative to the previous occurrence's line; `start` is
/// relative to the previous occurrence's start when on the same line, else
/// absolute. Typical files compress to well under the size of HTML.
pub fn pack(occurrences: &[Occurrence]) -> Vec<u32> {
    let mut out = Vec::with_capacity(occurrences.len() * 4);
    let (mut prev_line, mut prev_start) = (0u32, 0u32);
    for o in occurrences {
        let dl = o.line - prev_line;
        let start = if dl == 0 {
            o.start_col - prev_start
        } else {
            o.start_col
        };
        out.extend_from_slice(&[dl, start, o.end_col - o.start_col, o.kind as u32]);
        prev_line = o.line;
        prev_start = o.start_col;
    }
    out
}

/// Error from [`unpack`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnpackError {
    /// The array length is not a multiple of four.
    #[error("packed length {0} is not a multiple of 4")]
    Length(usize),
    /// An unknown kind value.
    #[error("unknown syntax kind {0}")]
    Kind(u32),
}

/// Inverse of [`pack`].
pub fn unpack(packed: &[u32]) -> Result<Vec<Occurrence>, UnpackError> {
    if packed.len() % 4 != 0 {
        return Err(UnpackError::Length(packed.len()));
    }
    let mut out = Vec::with_capacity(packed.len() / 4);
    let (mut line, mut start) = (0u32, 0u32);
    for q in packed.chunks_exact(4) {
        if q[0] == 0 {
            start += q[1];
        } else {
            line += q[0];
            start = q[1];
        }
        let kind = u8::try_from(q[3])
            .ok()
            .and_then(SyntaxKind::from_u8)
            .ok_or(UnpackError::Kind(q[3]))?;
        out.push(Occurrence {
            line,
            start_col: start,
            end_col: start + q[2],
            kind,
        });
    }
    Ok(out)
}

/// Renders `source` as an HTML table (one row per line, as the
/// `highlighted HTML` API returns it) with `<span class="hl-…">` for occurrences.
pub fn to_html(source: &str, occurrences: &[Occurrence]) -> String {
    let mut html = String::with_capacity(source.len() * 2 + 64);
    html.push_str("<table><tbody>");
    let mut occ = occurrences.iter().peekable();
    let body = source.strip_suffix('\n').unwrap_or(source);
    for (i, line) in body.split('\n').enumerate() {
        html.push_str(&format!(
            "<tr><td class=\"line\" data-line=\"{}\"></td><td class=\"code\"><div>",
            i + 1
        ));
        // Byte offset of each UTF-16 column on this line.
        let mut col_to_byte = Vec::with_capacity(line.len() + 1);
        for (b, ch) in line.char_indices() {
            for _ in 0..ch.len_utf16() {
                col_to_byte.push(b);
            }
        }
        col_to_byte.push(line.len());
        let byte = |c: u32| col_to_byte.get(c as usize).copied().unwrap_or(line.len());
        let mut at = 0usize;
        while let Some(o) = occ.peek().filter(|o| o.line as usize == i) {
            let (s, e) = (byte(o.start_col), byte(o.end_col));
            if s >= at {
                escape_into(&mut html, &line[at..s]);
                html.push_str("<span class=\"hl-");
                html.push_str(o.kind.css_class());
                html.push_str("\">");
                escape_into(&mut html, &line[s..e]);
                html.push_str("</span>");
                at = e;
            }
            occ.next();
        }
        escape_into(&mut html, &line[at..]);
        html.push_str("</div></td></tr>");
    }
    html.push_str("</tbody></table>");
    html
}

fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\r' => {}
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SyntaxKind as K;

    #[test]
    fn spans_split_on_lines_and_count_utf16() {
        // "é" is 2 bytes / 1 UTF-16 unit; "😀" is 4 bytes / 2 units.
        let src = "a é\"x😀y\"\n/* c\nd */ z".as_bytes();
        let s_start = src.iter().position(|&b| b == b'"').unwrap();
        let s_end = s_start + "\"x😀y\"".len();
        let c_start = src.iter().position(|&b| b == b'/').unwrap();
        let c_end = c_start + "/* c\nd */".len();
        let occ = spans_to_occurrences(
            src,
            &[(s_start, s_end, K::String), (c_start, c_end, K::Comment)],
        );
        assert_eq!(
            occ,
            vec![
                Occurrence {
                    line: 0,
                    start_col: 3,
                    end_col: 9,
                    kind: K::String
                },
                Occurrence {
                    line: 1,
                    start_col: 0,
                    end_col: 4,
                    kind: K::Comment
                },
                Occurrence {
                    line: 2,
                    start_col: 0,
                    end_col: 4,
                    kind: K::Comment
                },
            ]
        );
    }

    #[test]
    fn touching_spans_of_one_kind_merge() {
        let occ = spans_to_occurrences(
            b"abcdef",
            &[(0, 2, K::Keyword), (2, 4, K::Keyword), (4, 6, K::Type)],
        );
        assert_eq!(occ.len(), 2);
        assert_eq!((occ[0].start_col, occ[0].end_col), (0, 4));
    }

    #[test]
    fn pack_roundtrip() {
        let occ = vec![
            Occurrence {
                line: 0,
                start_col: 0,
                end_col: 2,
                kind: K::Keyword,
            },
            Occurrence {
                line: 0,
                start_col: 3,
                end_col: 7,
                kind: K::Function,
            },
            Occurrence {
                line: 4,
                start_col: 8,
                end_col: 9,
                kind: K::Number,
            },
        ];
        let packed = pack(&occ);
        assert_eq!(packed, [0, 0, 2, 2, 0, 3, 4, 9, 4, 8, 1, 6]);
        assert_eq!(unpack(&packed).unwrap(), occ);
        assert_eq!(unpack(&[1, 2, 3]), Err(UnpackError::Length(3)));
        assert_eq!(unpack(&[0, 0, 1, 99]), Err(UnpackError::Kind(99)));
    }

    #[test]
    fn html_escapes_and_wraps() {
        let src = "if a<b {\n  x\n}\n";
        let occ = [Occurrence {
            line: 0,
            start_col: 0,
            end_col: 2,
            kind: K::Keyword,
        }];
        let html = to_html(src, &occ);
        assert!(html.starts_with("<table><tbody><tr><td class=\"line\" data-line=\"1\"></td>"));
        assert!(html.contains("<span class=\"hl-keyword\">if</span> a&lt;b {"));
        assert_eq!(html.matches("<tr>").count(), 3);
    }
}
