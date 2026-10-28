//! Diffs between commits and compare views (commit and compare pages).
//!
//! Tree diffs and rename detection come from `gix`; line diffs use the
//! histogram algorithm with git's slider heuristics, so hunks match
//! `git diff` closely.

use std::ops::ControlFlow;

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics};
use gix::object::tree::diff::ChangeDetached;

use crate::error::{GitError, Result};
use crate::read::{self, CommitInfo, is_binary, normalize_path};

/// How a file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeStatus {
    /// New file.
    Added,
    /// Removed file.
    Deleted,
    /// Contents or mode changed.
    Modified,
    /// Moved (possibly with changes).
    Renamed,
    /// Copied from another file.
    Copied,
    /// File ↔ symlink ↔ submodule.
    TypeChanged,
}

/// One unified-diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based first line in the old file (0 for an empty side).
    pub old_start: u32,
    /// Lines from the old file.
    pub old_lines: u32,
    /// 1-based first line in the new file.
    pub new_start: u32,
    /// Lines from the new file.
    pub new_lines: u32,
    /// Lines prefixed with ` `, `+` or `-`, each newline-terminated; a missing
    /// final newline is marked with `\ No newline at end of file` like git.
    pub body: String,
    /// The text git prints after the `@@` header: the nearest line above the
    /// hunk in the old file that starts a "function" ([`section_heading`]).
    pub section: String,
}

/// The change to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// How the file changed.
    pub status: ChangeStatus,
    /// Path before (`None` when added).
    pub old_path: Option<String>,
    /// Path after (`None` when deleted).
    pub new_path: Option<String>,
    /// Blob before.
    pub old_oid: Option<ObjectId>,
    /// Blob after.
    pub new_oid: Option<ObjectId>,
    /// Mode before (0 when added).
    pub old_mode: u32,
    /// Mode after (0 when deleted).
    pub new_mode: u32,
    /// Added lines.
    pub additions: u32,
    /// Removed lines.
    pub deletions: u32,
    /// Either side is binary (no hunks or line stats).
    pub binary: bool,
    /// A side exceeded [`DiffOptions::max_blob_bytes`] (no hunks or line stats).
    pub too_large: bool,
    /// Hunks (empty with [`DiffOptions::stats_only`]).
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path to show for the file (new path, or old path when deleted).
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

/// Options for [`diff_commits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    /// Only files under these paths (exact file or directory prefix).
    pub paths: Vec<String>,
    /// Line counts only, no hunks.
    pub stats_only: bool,
    /// Paths, oids and modes only: no blob is read (no line counts, hunks or
    /// binary detection).
    pub headers_only: bool,
    /// Context lines around changes.
    pub context_lines: u32,
    /// Detect renames and copies.
    pub detect_renames: bool,
    /// Skip line diffs for blobs larger than this.
    pub max_blob_bytes: u64,
    /// Stop after this many files.
    pub max_files: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            stats_only: false,
            headers_only: false,
            context_lines: 3,
            detect_renames: true,
            max_blob_bytes: 2 * 1024 * 1024,
            max_files: 10_000,
        }
    }
}

/// Diffs `old` → `new`. With `old = None` the first parent of `new` is used
/// (the empty tree for a root commit). Files are emitted in path order.
/// Returns `true` if the listing stopped at `max_files`.
pub fn diff_commits(
    repo: &gix::Repository,
    old: Option<ObjectId>,
    new: ObjectId,
    opts: &DiffOptions,
    emit: impl FnMut(FileDiff) -> ControlFlow<()>,
) -> Result<bool> {
    diff_commits_where(repo, old, new, opts, |_| true, emit)
}

/// [`diff_commits`] restricted to the files `keep` accepts. `keep` sees the
/// header (paths, oids, modes) before any blob is read; rejected files do not
/// count towards [`DiffOptions::max_files`].
pub fn diff_commits_where(
    repo: &gix::Repository,
    old: Option<ObjectId>,
    new: ObjectId,
    opts: &DiffOptions,
    mut keep: impl FnMut(&FileDiff) -> bool,
    mut emit: impl FnMut(FileDiff) -> ControlFlow<()>,
) -> Result<bool> {
    let new_commit = read::find_commit(repo, new)?;
    let old_commit = match old {
        Some(id) => Some(read::find_commit(repo, id)?),
        None => match new_commit.parent_ids().next() {
            Some(p) => Some(read::find_commit(repo, p.detach())?),
            None => None,
        },
    };
    let new_tree = new_commit.tree()?;
    let old_tree = match &old_commit {
        Some(c) => c.tree()?,
        None => repo.empty_tree(),
    };
    let filters: Vec<String> = opts
        .paths
        .iter()
        .map(|p| normalize_path(p))
        .collect::<Result<_>>()?;
    let rewrites = opts.detect_renames.then(gix::diff::Rewrites::default);
    let options = gix::diff::Options::default().with_rewrites(rewrites);
    let mut changes = repo.diff_tree_to_tree(&old_tree, &new_tree, options)?;
    changes.sort_by(|a, b| change_sort_key(a).cmp(change_sort_key(b)));

    let mut count = 0usize;
    for change in changes {
        let Some(mut file) = file_diff_header(&change) else {
            continue;
        };
        if !filters.is_empty()
            && !filters.iter().any(|f| {
                file.old_path.as_deref().is_some_and(|p| under(p, f))
                    || file.new_path.as_deref().is_some_and(|p| under(p, f))
            })
        {
            continue;
        }
        if !keep(&file) {
            continue;
        }
        if count == opts.max_files {
            return Ok(true);
        }
        count += 1;
        if !opts.headers_only {
            fill_lines(repo, &mut file, opts)?;
        }
        if emit(file).is_break() {
            return Ok(false);
        }
    }
    Ok(false)
}

/// Collects [`diff_commits`] into a vector.
pub fn diff_commits_vec(
    repo: &gix::Repository,
    old: Option<ObjectId>,
    new: ObjectId,
    opts: &DiffOptions,
) -> Result<(Vec<FileDiff>, bool)> {
    let mut out = Vec::new();
    let truncated = diff_commits(repo, old, new, opts, |f| {
        out.push(f);
        ControlFlow::Continue(())
    })?;
    Ok((out, truncated))
}

fn under(path: &str, filter: &str) -> bool {
    filter.is_empty()
        || path == filter
        || (path.starts_with(filter) && path.as_bytes().get(filter.len()) == Some(&b'/'))
}

fn change_sort_key(c: &ChangeDetached) -> &[u8] {
    match c {
        ChangeDetached::Addition { location, .. }
        | ChangeDetached::Deletion { location, .. }
        | ChangeDetached::Modification { location, .. }
        | ChangeDetached::Rewrite { location, .. } => location.as_slice(),
    }
}

/// Converts a tree change into a header without line data. Directory entries
/// (the tree diff reports them alongside their contents) yield `None`.
fn file_diff_header(change: &ChangeDetached) -> Option<FileDiff> {
    let path = |l: &gix::bstr::BString| Some(l.to_str_lossy().into_owned());
    let blank = |status| FileDiff {
        status,
        old_path: None,
        new_path: None,
        old_oid: None,
        new_oid: None,
        old_mode: 0,
        new_mode: 0,
        additions: 0,
        deletions: 0,
        binary: false,
        too_large: false,
        hunks: Vec::new(),
    };
    let mode = |m: gix::objs::tree::EntryMode| u32::from(m.value());
    Some(match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            FileDiff {
                new_path: path(location),
                new_oid: Some(*id),
                new_mode: mode(*entry_mode),
                ..blank(ChangeStatus::Added)
            }
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            FileDiff {
                old_path: path(location),
                old_oid: Some(*id),
                old_mode: mode(*entry_mode),
                ..blank(ChangeStatus::Deleted)
            }
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            if entry_mode.is_tree() && previous_entry_mode.is_tree() {
                return None;
            }
            let status = if previous_entry_mode.kind() == entry_mode.kind()
                || (previous_entry_mode.is_blob() && entry_mode.is_blob())
            {
                ChangeStatus::Modified
            } else {
                ChangeStatus::TypeChanged
            };
            FileDiff {
                old_path: path(location),
                new_path: path(location),
                old_oid: Some(*previous_id),
                new_oid: Some(*id),
                old_mode: mode(*previous_entry_mode),
                new_mode: mode(*entry_mode),
                ..blank(status)
            }
        }
        ChangeDetached::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            entry_mode,
            id,
            location,
            copy,
            ..
        } => {
            if entry_mode.is_tree() {
                return None;
            }
            FileDiff {
                old_path: path(source_location),
                new_path: path(location),
                old_oid: Some(*source_id),
                new_oid: Some(*id),
                old_mode: mode(*source_entry_mode),
                new_mode: mode(*entry_mode),
                ..blank(if *copy {
                    ChangeStatus::Copied
                } else {
                    ChangeStatus::Renamed
                })
            }
        }
    })
}

/// Fills line stats and hunks for blob changes.
fn fill_lines(repo: &gix::Repository, file: &mut FileDiff, opts: &DiffOptions) -> Result<()> {
    let is_blob_mode = |m: u32| m == 0 || (m & 0o170000) == 0o100000 || (m & 0o170000) == 0o120000;
    if !is_blob_mode(file.old_mode) || !is_blob_mode(file.new_mode) {
        return Ok(()); // submodule changes have no lines
    }
    if file.old_oid == file.new_oid {
        return Ok(()); // pure rename or mode change
    }
    let load = |oid: Option<ObjectId>| -> Result<Option<Vec<u8>>> {
        let Some(oid) = oid else {
            return Ok(Some(Vec::new()));
        };
        if repo.find_header(oid)?.size() > opts.max_blob_bytes {
            return Ok(None);
        }
        Ok(Some(repo.find_blob(oid)?.take_data()))
    };
    let (Some(old), Some(new)) = (load(file.old_oid)?, load(file.new_oid)?) else {
        file.too_large = true;
        return Ok(());
    };
    if is_binary(&old) || is_binary(&new) {
        file.binary = true;
        return Ok(());
    }
    let input = InternedInput::new(old.as_slice(), new.as_slice());
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    file.additions = diff.count_additions();
    file.deletions = diff.count_removals();
    if !opts.stats_only {
        file.hunks = UnifiedDiff::new(
            &diff,
            &input,
            HunkCollector::default(),
            ContextSize::symmetrical(opts.context_lines),
        )
        .consume()
        .map_err(GitError::Io)?;
        let lines: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
        for h in &mut file.hunks {
            // 0-based first old line of the hunk; a pure insertion's
            // `old_start` is the line it follows.
            let first = if h.old_lines > 0 {
                h.old_start.saturating_sub(1)
            } else {
                h.old_start
            };
            h.section = section_heading(&lines[..(first as usize).min(lines.len())]);
        }
    }
    Ok(())
}

/// Git's default hunk heading (no `diff` driver configured): the last of
/// `lines` that starts with a letter, `_` or `$`, cut to 80 bytes, without
/// trailing whitespace. Empty if there is none.
pub fn section_heading(lines: &[&[u8]]) -> String {
    let Some(line) = lines.iter().rev().find(|l| {
        l.first()
            .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
    }) else {
        return String::new();
    };
    let line = &line[..line.len().min(80)];
    let end = line
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |i| i + 1);
    line[..end].to_str_lossy().into_owned()
}

#[derive(Default)]
struct HunkCollector {
    hunks: Vec<Hunk>,
}

impl ConsumeHunk for HunkCollector {
    type Out = Vec<Hunk>;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        let mut body = String::new();
        for (kind, line) in lines {
            body.push(match kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Add => '+',
                DiffLineKind::Remove => '-',
            });
            body.push_str(&line.to_str_lossy());
            if !line.ends_with(b"\n") {
                body.push_str("\n\\ No newline at end of file\n");
            }
        }
        self.hunks.push(Hunk {
            old_start: header.before_hunk_start,
            old_lines: header.before_hunk_len,
            new_start: header.after_hunk_start,
            new_lines: header.after_hunk_len,
            body,
            section: String::new(),
        });
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.hunks
    }
}

/// Result of [`compare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// Best common ancestor of base and head (`None` for unrelated histories).
    pub merge_base: Option<ObjectId>,
    /// Commits reachable from head but not base, newest first.
    pub commits: Vec<CommitInfo>,
    /// Whether `commits` stopped at the limit.
    pub commits_truncated: bool,
    /// Files changed merge-base..head (stats only).
    pub files: Vec<FileDiff>,
}

/// The compare view `base...head`: commits in `base..head` and the files
/// changed since the merge base (like a merge request diff).
pub fn compare(
    repo: &gix::Repository,
    base: ObjectId,
    head: ObjectId,
    max_commits: usize,
) -> Result<Comparison> {
    read::find_commit(repo, base)?;
    read::find_commit(repo, head)?;
    let merge_base = crate::graph::merge_base(repo, base, head)?;

    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;
    let walk = repo
        .rev_walk([head])
        .with_hidden([base])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .use_commit_graph(true)
        .all()?;
    let mut commits = Vec::new();
    let mut commits_truncated = false;
    for info in walk {
        let info = info.map_err(GitError::gix)?;
        if commits.len() == max_commits {
            commits_truncated = true;
            break;
        }
        commits.push(read::commit_info(repo, info.id)?);
    }

    let files = match merge_base {
        Some(mb) => {
            let opts = DiffOptions {
                stats_only: true,
                ..Default::default()
            };
            diff_commits_vec(repo, Some(mb), head, &opts)?.0
        }
        None => Vec::new(),
    };
    Ok(Comparison {
        merge_base,
        commits,
        commits_truncated,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_filter_matches_files_and_dirs() {
        assert!(under("src/main.rs", "src"));
        assert!(under("src/main.rs", "src/main.rs"));
        assert!(!under("srcfoo/main.rs", "src"));
        assert!(under("anything", ""));
    }
}
