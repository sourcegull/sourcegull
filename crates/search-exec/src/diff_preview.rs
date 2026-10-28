//! Diff match previews.
//!
//! A matched commit shows at most 5 files, 3 hunks per file, and per hunk
//! the matching changed lines with one line of context, at most 7 lines;
//! further matches in a hunk are counted in its section as `... +N`.
//! Without highlights (no pattern) every changed line counts as matching.
//! Line numbers stay exact for hunks split from the same git hunk.

use std::ops::Range;

use crate::commits::{DiffFileMatch, DiffHunkMatch};

/// Lines of context kept on each side of a matching line.
const CONTEXT: usize = 1;
/// Lines one preview hunk holds at most.
const MAX_HUNK_LINES: u32 = 7;
/// Preview hunks kept per file.
const MAX_HUNKS: usize = 3;
/// Files kept per commit.
const MAX_FILES: usize = 5;

fn hunk_highlighted(h: &DiffHunkMatch) -> bool {
    !h.ranges.is_empty()
}

fn file_highlighted(f: &DiffFileMatch) -> bool {
    f.hunks.iter().any(hunk_highlighted)
}

/// Trims a matched commit's files to their preview.
pub(crate) fn preview(files: Vec<DiffFileMatch>) -> Vec<DiffFileMatch> {
    let any_highlighted = files.iter().any(file_highlighted);
    files
        .into_iter()
        .filter(|f| !any_highlighted || file_highlighted(f))
        .take(MAX_FILES)
        .map(|f| {
            let mut pieces: Vec<DiffHunkMatch> = f.hunks.iter().flat_map(split_hunk).collect();
            if pieces.iter().any(hunk_highlighted) {
                pieces.retain(hunk_highlighted);
            }
            pieces.truncate(MAX_HUNKS);
            DiffFileMatch {
                old_path: f.old_path,
                new_path: f.new_path,
                hunks: pieces,
            }
        })
        .collect()
}

/// One line of a hunk body with the ranges that start on it, relative to
/// the line.
struct Line<'a> {
    text: &'a str,
    ranges: Vec<Range<usize>>,
}

impl Line<'_> {
    fn added(&self) -> bool {
        self.text.starts_with('+')
    }
    fn removed(&self) -> bool {
        self.text.starts_with('-')
    }
}

/// Splits a hunk body into lines (each with its newline) and hands each
/// range to the line it starts on.
fn lines_of(h: &DiffHunkMatch) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut ranges = h.ranges.iter().peekable();
    for text in h.body.split_inclusive('\n') {
        let end = start + text.len();
        let mut own = Vec::new();
        while let Some(r) = ranges.next_if(|r| r.start < end) {
            let from = r.start.saturating_sub(start);
            own.push(from..from + (r.end - r.start));
        }
        lines.push(Line { text, ranges: own });
        start = end;
    }
    lines
}

/// Cuts a hunk into the pieces a preview shows: runs of matching changed
/// lines and their context, each at most `MAX_HUNK_LINES` lines, with line
/// numbers counted from the hunk's start.
fn split_hunk(h: &DiffHunkMatch) -> Vec<DiffHunkMatch> {
    let lines = lines_of(h);
    let no_ranges = h.ranges.is_empty();
    let matching: Vec<bool> = lines
        .iter()
        .map(|l| (l.added() || l.removed()) && (no_ranges || !l.ranges.is_empty()))
        .collect();
    let mut context = vec![false; lines.len()];
    for (i, _) in matching.iter().enumerate().filter(|(_, m)| **m) {
        let lo = i.saturating_sub(CONTEXT);
        let hi = (i + CONTEXT).min(lines.len() - 1);
        for (j, c) in context.iter_mut().enumerate().take(hi + 1).skip(lo) {
            if j != i {
                *c = true;
            }
        }
    }

    let mut out = Vec::new();
    let mut current: Option<(DiffHunkMatch, u32)> = None;
    let close = |piece: Option<(DiffHunkMatch, u32)>, out: &mut Vec<DiffHunkMatch>| {
        if let Some((mut piece, extra)) = piece {
            if extra > 0 {
                if !piece.section.is_empty() {
                    piece.section.push(' ');
                }
                piece.section.push_str(&format!("... +{extra}"));
            }
            out.push(piece);
        }
    };
    let (mut old_offset, mut new_offset) = (0u32, 0u32);
    for (i, line) in lines.iter().enumerate() {
        if matching[i] || context[i] {
            let (piece, extra) = current.get_or_insert_with(|| {
                (
                    DiffHunkMatch {
                        old_start: h.old_start + old_offset,
                        old_lines: 0,
                        new_start: h.new_start + new_offset,
                        new_lines: 0,
                        body: String::new(),
                        ranges: Vec::new(),
                        section: h.section.clone(),
                    },
                    0,
                )
            });
            if piece.body.split_inclusive('\n').count() as u32 >= MAX_HUNK_LINES {
                if matching[i] {
                    *extra += 1;
                }
            } else {
                let shift = piece.body.len();
                piece.body.push_str(line.text);
                piece
                    .ranges
                    .extend(line.ranges.iter().map(|r| r.start + shift..r.end + shift));
                if !line.added() {
                    piece.old_lines += 1;
                }
                if !line.removed() {
                    piece.new_lines += 1;
                }
            }
        } else {
            close(current.take(), &mut out);
        }
        if !line.added() {
            old_offset += 1;
        }
        if !line.removed() {
            new_offset += 1;
        }
    }
    close(current.take(), &mut out);
    out
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn hunk(body: &str, ranges: Vec<Range<usize>>) -> DiffHunkMatch {
        DiffHunkMatch {
            old_start: 10,
            old_lines: 0,
            new_start: 20,
            new_lines: 0,
            body: body.into(),
            ranges,
            section: "func f() {".into(),
        }
    }

    fn range_of(body: &str, needle: &str, nth: usize) -> Range<usize> {
        let at = body.match_indices(needle).nth(nth).unwrap().0;
        at..at + needle.len()
    }

    #[test]
    fn keeps_matching_lines_with_one_line_of_context() {
        let body = " a\n b\n c\n-old x\n+new x\n d\n e\n f\n+other\n g\n";
        let h = hunk(body, vec![range_of(body, "x", 1)]);
        let split = split_hunk(&h);
        assert_eq!(split.len(), 1);
        let s = &split[0];
        // The changed line before the match is its context; " c" is not.
        assert_eq!(s.body, "-old x\n+new x\n d\n");
        assert_eq!(
            (s.old_start, s.old_lines, s.new_start, s.new_lines),
            (13, 2, 23, 2)
        );
        assert_eq!(&s.body[s.ranges[0].clone()], "x");
        assert_eq!(s.section, "func f() {");
    }

    #[test]
    fn without_highlights_every_change_matches() {
        let body = " a\n-b\n c\n d\n e\n+f\n";
        let split = split_hunk(&hunk(body, Vec::new()));
        let bodies: Vec<_> = split.iter().map(|h| h.body.as_str()).collect();
        assert_eq!(bodies, [" a\n-b\n c\n", " e\n+f\n"]);
        assert_eq!((split[1].old_start, split[1].new_start), (14, 23));
    }

    #[test]
    fn long_runs_are_cut_and_counted() {
        let body: String = (0..10).map(|i| format!("+m{i}\n")).collect();
        let ranges = (0..10).map(|i| range_of(&body, "m", i)).collect();
        let split = split_hunk(&hunk(&body, ranges));
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].body.lines().count(), 7);
        assert_eq!(split[0].ranges.len(), 7);
        assert_eq!(split[0].section, "func f() { ... +3");
    }

    #[test]
    fn preview_limits_files_and_hunks() {
        let body = "+x\n a\n b\n c\n+x\n d\n e\n f\n+x\n g\n h\n i\n+x\n";
        let ranges: Vec<_> = (0..4).map(|i| range_of(body, "x", i)).collect();
        let file = |hl: bool| DiffFileMatch {
            old_path: Some("a".into()),
            new_path: Some("a".into()),
            hunks: vec![hunk(body, if hl { ranges.clone() } else { Vec::new() })],
        };
        let mut files = vec![file(false)];
        files.extend((0..6).map(|_| file(true)));
        let p = preview(files);
        // The file without highlights is dropped, then 5 files are kept.
        assert_eq!(p.len(), 5);
        assert!(p.iter().all(|f| f.hunks.len() == 3));
    }

    fn at(
        old_start: u32,
        new_start: u32,
        section: &str,
        body: &str,
        ranges: Vec<Range<usize>>,
    ) -> DiffHunkMatch {
        DiffHunkMatch {
            old_start,
            old_lines: 0,
            new_start,
            new_lines: 0,
            body: body.into(),
            ranges,
            section: section.into(),
        }
    }

    fn counts(h: &DiffHunkMatch) -> (u32, u32, u32, u32) {
        (h.old_start, h.old_lines, h.new_start, h.new_lines)
    }

    #[test]
    fn unhighlighted_change_next_to_a_match_is_context() {
        let body = " keep\n-red\n+blue\n same\n other\n";
        let split = split_hunk(&at(
            40,
            41,
            "class Paint",
            body,
            vec![range_of(body, "blue", 0)],
        ));
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].body, "-red\n+blue\n same\n");
        assert_eq!(counts(&split[0]), (41, 2, 42, 2));
        assert_eq!(split[0].ranges, [6..10]);
        assert_eq!(split[0].section, "class Paint");
    }

    #[test]
    fn separate_changes_without_ranges_are_separate_pieces() {
        let body = " one\n+two\n three\n four\n five\n-six\n seven\n";
        let split = split_hunk(&at(1, 1, "", body, Vec::new()));
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].body, " one\n+two\n three\n");
        assert_eq!(counts(&split[0]), (1, 2, 1, 3));
        assert_eq!(split[1].body, " five\n-six\n seven\n");
        assert_eq!(counts(&split[1]), (4, 3, 5, 2));

        let split = split_hunk(&at(1, 1, "", body, vec![range_of(body, "six", 0)]));
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].body, " five\n-six\n seven\n");
        assert_eq!(counts(&split[0]), (4, 3, 5, 2));
        assert_eq!(split[0].ranges, [7..10]);
    }

    #[test]
    fn overflow_is_counted_in_an_empty_section() {
        let body: String = (0..9).map(|i| format!("-k{i}\n")).collect();
        let ranges = (0..9).map(|i| range_of(&body, "k", i)).collect();
        let split = split_hunk(&at(100, 100, "", &body, ranges));
        assert_eq!(split.len(), 1);
        let want: String = (0..7).map(|i| format!("-k{i}\n")).collect();
        assert_eq!(split[0].body, want);
        assert_eq!(counts(&split[0]), (100, 7, 100, 0));
        assert_eq!(split[0].ranges.len(), 7);
        assert_eq!(split[0].section, "... +2");
    }

    #[test]
    fn whole_small_hunks_and_missing_final_newline() {
        let body = " ctx\n+zz\n+zz\n ctx\n";
        let ranges = vec![range_of(body, "zz", 0), range_of(body, "zz", 1)];
        let split = split_hunk(&at(5, 8, "mod util", body, ranges));
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].body, body);
        assert_eq!(counts(&split[0]), (5, 2, 8, 4));
        assert_eq!(split[0].ranges, [6..8, 10..12]);
        assert_eq!(split[0].section, "mod util");

        let body = "+alpha\n+beta";
        let split = split_hunk(&at(1, 1, "", body, vec![range_of(body, "ta", 0)]));
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].body, body);
        assert_eq!(counts(&split[0]), (1, 0, 1, 2));
        assert_eq!(split[0].ranges, [10..12]);
    }

    #[test]
    fn ranges_on_unchanged_lines_match_nothing() {
        let body = " a\n b\n+c\n d\n e\n";
        let split = split_hunk(&at(3, 3, "", body, vec![1..2]));
        assert!(split.is_empty());
    }

    #[test]
    fn preview_keeps_the_first_highlighted_files() {
        let body = "+x\n a\n b\n c\n+x\n d\n e\n f\n+x\n g\n h\n i\n+x\n";
        let ranges: Vec<_> = (0..4).map(|i| range_of(body, "x", i)).collect();
        let file = |n: usize, hl: bool| DiffFileMatch {
            old_path: Some(format!("f{n}")),
            new_path: Some(format!("f{n}")),
            hunks: vec![hunk(body, if hl { ranges.clone() } else { Vec::new() })],
        };
        let files: Vec<_> = (0..8).map(|n| file(n, n > 0)).collect();
        let p = preview(files);
        let paths: Vec<_> = p.iter().map(|f| f.new_path.as_deref().unwrap()).collect();
        assert_eq!(paths, ["f1", "f2", "f3", "f4", "f5"]);
        assert!(p.iter().all(|f| f.hunks.len() == 3));
    }
}
