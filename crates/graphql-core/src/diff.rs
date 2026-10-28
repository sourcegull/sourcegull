//! Diff value types shared by repository comparisons and bulk change previews: `DiffStat`
//! and `FileDiffHunk`.

use async_graphql::{Object, SimpleObject};
use sourcegull_rpc::repo::v1 as pb;

/// Converts a count to a GraphQL `Int`, saturating.
fn int(n: impl TryInto<i32>) -> i32 {
    n.try_into().unwrap_or(i32::MAX)
}

/// Added and deleted line counts.
#[derive(SimpleObject, Clone, Copy, Default)]
pub struct DiffStat {
    pub added: i32,
    pub deleted: i32,
    /// Always 0 (kept for compatibility; changes count as one deletion and one addition).
    pub changed: i32,
}

/// A line range of a hunk.
#[derive(SimpleObject)]
pub struct FileDiffHunkRange {
    pub start_line: i32,
    pub lines: i32,
}

/// A hunk of a file diff.
pub struct FileDiffHunk(pub pb::Hunk);

#[Object]
impl FileDiffHunk {
    /// The old line range.
    async fn old_range(&self) -> FileDiffHunkRange {
        FileDiffHunkRange {
            start_line: int(self.0.old_start),
            lines: int(self.0.old_lines),
        }
    }

    /// The new line range.
    async fn new_range(&self) -> FileDiffHunkRange {
        FileDiffHunkRange {
            start_line: int(self.0.new_start),
            lines: int(self.0.new_lines),
        }
    }

    /// Whether the old side lacks a final newline.
    async fn old_no_newline_at(&self) -> bool {
        self.0.body.contains("\n\\ No newline at end of file")
    }

    /// The `@@` section heading (git's function-context line), if any.
    async fn section(&self) -> Option<&str> {
        Some(self.0.section.as_str()).filter(|s| !s.is_empty())
    }

    /// The unified-diff body: lines prefixed with ` `, `+` or `-`.
    async fn body(&self) -> &str {
        &self.0.body
    }
}
