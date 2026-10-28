//! `PreviewRepositoryComparison`: the diff of a changeset spec, parsed from the unified diff
//! stored with it (the commit does not exist yet), as `fileDiffs` with hunks and stats.

use std::sync::Arc;

use async_graphql::{Context, Error, Object, Result, SimpleObject};
use sourcegull_batches::store::ChangesetSpec;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_types::Repo;

use sourcegull_graphql_core::diff::{DiffStat, FileDiffHunk};
use sourcegull_graphql_core::git::{BLOB_CONTENT_MAX_BYTES, git_error};
use sourcegull_graphql_core::shared::{
    ContentLoader, File2Data, File2Ref, GitBlobData, GitBlobRef, RepositoryRef, VirtualFileData,
    VirtualFileRef,
};
use sourcegull_services::git::GitReader;

use crate::context::{services, to_int};
use crate::pagination::{PageInfo, decode_cursor};

/// One file of a parsed unified diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedFileDiff {
    /// Path before the change (`None` for added files).
    pub old_path: Option<String>,
    /// Path after the change (`None` for deleted files).
    pub new_path: Option<String>,
    pub hunks: Vec<pb::Hunk>,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
}

/// The path of a `---`/`+++` line: `None` for `/dev/null`, without the `a/`/`b/` prefix and a
/// trailing tab with a timestamp.
fn header_path(rest: &str, prefix: &str) -> Option<String> {
    let p = rest.split('\t').next().unwrap_or(rest).trim_end();
    if p == "/dev/null" {
        return None;
    }
    let p = p.trim_matches('"');
    Some(p.strip_prefix(prefix).unwrap_or(p).to_owned())
}

/// `-a,b` or `+c,d` of a hunk header (a missing count is 1).
fn range(s: &str) -> Option<(u32, u32)> {
    let s = &s[1..];
    match s.split_once(',') {
        Some((start, n)) => Some((start.parse().ok()?, n.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// Parses `@@ -a,b +c,d @@ section`.
fn hunk_header(line: &str) -> Option<pb::Hunk> {
    let rest = line.strip_prefix("@@ ")?;
    let (ranges, section) = rest.split_once(" @@").unwrap_or((rest, ""));
    let mut parts = ranges.split_whitespace();
    let (old_start, old_lines) = range(parts.next().filter(|p| p.starts_with('-'))?)?;
    let (new_start, new_lines) = range(parts.next().filter(|p| p.starts_with('+'))?)?;
    Some(pb::Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        body: String::new(),
        section: section.trim().to_owned(),
    })
}

/// Parses a (git) unified diff into its files. Lines of a hunk are counted against its
/// header, so a removed line that starts with `--` is not taken for a file header.
pub fn parse(diff: &str) -> Vec<ParsedFileDiff> {
    let mut files: Vec<ParsedFileDiff> = Vec::new();
    // Old and new lines still expected in the current hunk.
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in diff.split_inclusive('\n') {
        let text = line.trim_end_matches('\n').trim_end_matches('\r');
        let in_hunk = old_left > 0 || new_left > 0;
        // Some tools drop the space of empty context lines.
        let body = match text.chars().next() {
            Some('+' | '-' | ' ' | '\\') => Some(text),
            None => Some(" "),
            // A malformed hunk ends here.
            Some(_) => None,
        };
        // `\ No newline at end of file` may follow the last line of a hunk.
        if !(in_hunk || text.starts_with('\\')) || body.is_none() {
            old_left = 0;
            new_left = 0;
        } else if let (Some(body), Some(file)) = (body, files.last_mut()) {
            match body.as_bytes()[0] {
                b'+' => {
                    new_left = new_left.saturating_sub(1);
                    file.added += 1;
                }
                b'-' => {
                    old_left = old_left.saturating_sub(1);
                    file.deleted += 1;
                }
                b' ' => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                _ => {}
            }
            if let Some(hunk) = file.hunks.last_mut() {
                hunk.body.push_str(body);
                hunk.body.push('\n');
            }
            continue;
        }
        if let Some(rest) = text.strip_prefix("diff --git ") {
            let (old, new) = rest
                .split_once(" b/")
                .map(|(a, b)| (a.strip_prefix("a/").unwrap_or(a), b))
                .unwrap_or((rest, rest));
            files.push(ParsedFileDiff {
                old_path: Some(old.to_owned()),
                new_path: Some(new.to_owned()),
                ..Default::default()
            });
        } else if let Some(rest) = text.strip_prefix("--- ") {
            // A diff without `diff --git` headers starts its files here.
            if files.last().is_none_or(|f| !f.hunks.is_empty()) {
                files.push(ParsedFileDiff::default());
            }
            if let Some(f) = files.last_mut() {
                f.old_path = header_path(rest, "a/");
            }
        } else if let Some(rest) = text.strip_prefix("+++ ") {
            if let Some(f) = files.last_mut() {
                f.new_path = header_path(rest, "b/");
            }
        } else if let Some(rest) = text.strip_prefix("rename from ") {
            if let Some(f) = files.last_mut() {
                f.old_path = Some(rest.to_owned());
            }
        } else if let Some(rest) = text.strip_prefix("rename to ") {
            if let Some(f) = files.last_mut() {
                f.new_path = Some(rest.to_owned());
            }
        } else if text.starts_with("new file mode") {
            if let Some(f) = files.last_mut() {
                f.old_path = None;
            }
        } else if text.starts_with("deleted file mode") {
            if let Some(f) = files.last_mut() {
                f.new_path = None;
            }
        } else if text.starts_with("Binary files ") || text == "GIT binary patch" {
            if let Some(f) = files.last_mut() {
                f.binary = true;
            }
        } else if let Some(h) = hunk_header(text)
            && let Some(f) = files.last_mut()
        {
            old_left = h.old_lines;
            new_left = h.new_lines;
            f.hunks.push(h);
        }
    }
    files
}

/// The file's content after applying the hunks of its diff to `old`. Hunks are applied in order at
/// their old line numbers; lines between and after them are kept. A `\ No newline at end of file`
/// marker after an added or context line leaves that line without its line break.
pub fn apply_hunks(old: &str, hunks: &[pb::Hunk]) -> std::result::Result<String, String> {
    let lines: Vec<&str> = old.split_inclusive('\n').collect();
    let mut out = String::with_capacity(old.len());
    // The next old line to copy (0-based).
    let mut next = 0usize;
    let malformed = |what: &str| Err(format!("malformed patch: {what}"));
    for h in hunks {
        // A hunk that only adds lines inserts them after line `old_start`.
        let start = if h.old_lines == 0 {
            h.old_start as usize
        } else {
            (h.old_start as usize).saturating_sub(1)
        };
        if start < next || start > lines.len() {
            return malformed("hunks overlap or start past the end of the file");
        }
        for l in &lines[next..start] {
            out.push_str(l);
        }
        next = start;
        // Whether the last body line wrote to the output.
        let mut wrote = false;
        for line in h.body.lines() {
            match line.as_bytes().first() {
                Some(b' ') | None => {
                    let Some(l) = lines.get(next) else {
                        return malformed("a context line is past the end of the file");
                    };
                    out.push_str(l);
                    next += 1;
                    wrote = true;
                }
                Some(b'-') => {
                    if next >= lines.len() {
                        return malformed("a removed line is past the end of the file");
                    }
                    next += 1;
                    wrote = false;
                }
                Some(b'+') => {
                    out.push_str(&line[1..]);
                    out.push('\n');
                    wrote = true;
                }
                Some(b'\\') => {
                    if wrote && out.ends_with('\n') {
                        out.pop();
                        if out.ends_with('\r') {
                            out.pop();
                        }
                    }
                }
                Some(_) => return malformed(&format!("unexpected hunk line {line:?}")),
            }
        }
    }
    for l in &lines[next.min(lines.len())..] {
        out.push_str(l);
    }
    Ok(out)
}

/// The diff of a changeset spec.
pub struct PreviewRepositoryComparison {
    spec: Arc<ChangesetSpec>,
    repo: Arc<Repo>,
}

impl PreviewRepositoryComparison {
    pub fn new(spec: Arc<ChangesetSpec>, repo: Arc<Repo>) -> Self {
        Self { spec, repo }
    }
}

/// One file of a changeset spec's diff.
pub struct PreviewFileDiff {
    f: ParsedFileDiff,
    /// The repository and the commit the diff applies to (none without a base revision).
    base: Option<Base>,
}

/// The repository and revision a changeset spec's diff applies to.
#[derive(Clone)]
struct Base {
    repo: Arc<Repo>,
    git: GitReader,
    rev: String,
}

impl PreviewFileDiff {
    fn old_blob(&self) -> Option<GitBlobData> {
        let base = self.base.as_ref()?;
        let path = self.f.old_path.as_deref()?;
        Some(GitBlobData {
            repo: (*base.repo).clone(),
            commit_oid: base.rev.clone(),
            rev: None,
            path: path.to_owned(),
        })
    }

    /// The new file, its content computed when asked for.
    fn new_virtual_file(&self) -> Option<VirtualFileData> {
        let path = self.f.new_path.as_deref()?;
        let old = self
            .base
            .as_ref()
            .zip(self.f.old_path.clone())
            .map(|(base, path)| (base.git.clone(), base.rev.clone(), path));
        let missing_base = old.is_none() && self.f.old_path.is_some();
        let (binary, hunks) = (self.f.binary, Arc::new(self.f.hunks.clone()));
        let load: ContentLoader = Arc::new(move || {
            let (old, hunks) = (old.clone(), hunks.clone());
            Box::pin(async move {
                if binary {
                    return Ok(String::new());
                }
                if missing_base {
                    return Err(Error::new("the changeset spec has no base revision"));
                }
                let old = match old {
                    Some((git, rev, path)) => {
                        let f = git
                            .read_file(&rev, &path, BLOB_CONTENT_MAX_BYTES)
                            .await
                            .map_err(git_error)?;
                        if f.binary {
                            String::new()
                        } else {
                            String::from_utf8_lossy(&f.content).into_owned()
                        }
                    }
                    None => String::new(),
                };
                apply_hunks(&old, &hunks).map_err(Error::new)
            })
        });
        // Not in the repository yet, so it has no page.
        Some(VirtualFileData {
            path: path.to_owned(),
            url: String::new(),
            loader: load,
        })
    }
}

#[Object]
impl PreviewFileDiff {
    /// Path before the change (null for added files).
    async fn old_path(&self) -> Option<&str> {
        self.f.old_path.as_deref()
    }

    /// The file before the change, at the changeset spec's base revision (null for added
    /// files).
    async fn old_file(&self) -> Option<GitBlobRef> {
        self.old_blob().map(GitBlobRef)
    }

    /// Path after the change (null for deleted files).
    async fn new_path(&self) -> Option<&str> {
        self.f.new_path.as_deref()
    }

    /// The file after the change: the old file with the diff applied (null for deleted
    /// files). Its URL is empty, as the file exists nowhere yet.
    async fn new_file(&self) -> Option<VirtualFileRef> {
        self.new_virtual_file().map(VirtualFileRef)
    }

    /// The new file, or the old one for deletions.
    async fn most_relevant_file(&self) -> Option<File2Ref> {
        match self.new_virtual_file() {
            Some(f) => Some(File2Ref(File2Data::VirtualFile(f))),
            None => self
                .old_blob()
                .map(|b| File2Ref(File2Data::GitBlob(Box::new(b)))),
        }
    }

    /// The hunks.
    async fn hunks(&self) -> Vec<FileDiffHunk> {
        self.f.hunks.iter().cloned().map(FileDiffHunk).collect()
    }

    /// Line counts.
    async fn stat(&self) -> DiffStat {
        DiffStat {
            added: to_int(i64::from(self.f.added)),
            deleted: to_int(i64::from(self.f.deleted)),
            changed: 0,
        }
    }

    /// Whether either side is binary.
    async fn binary(&self) -> bool {
        self.f.binary
    }

    /// A stable id within the diff.
    #[graphql(name = "internalID")]
    async fn internal_id(&self) -> String {
        let old = self.f.old_path.as_deref().unwrap_or("");
        let new = self.f.new_path.as_deref().unwrap_or(old);
        format!("{old}:{new}")
    }
}

/// A page of a changeset spec's file diffs.
#[derive(SimpleObject)]
pub struct PreviewFileDiffConnection {
    pub nodes: Vec<PreviewFileDiff>,
    pub total_count: i32,
    pub page_info: PageInfo,
    /// Totals over all (matching) files.
    pub diff_stat: DiffStat,
    /// The unified diff as uploaded.
    pub raw_diff: String,
}

/// Whether `path` is one of `paths` or inside one of them.
fn path_matches(path: &str, paths: &[String]) -> bool {
    paths.iter().any(|p| {
        let p = p.trim_end_matches('/');
        p.is_empty()
            || path == p
            || path
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[Object]
impl PreviewRepositoryComparison {
    /// The repository the changeset is created in.
    async fn base_repository(&self) -> RepositoryRef {
        RepositoryRef((*self.repo).clone())
    }

    /// The changed files with hunks, in diff order; with `paths`, only the files at or under
    /// those paths.
    async fn file_diffs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        paths: Option<Vec<String>>,
    ) -> Result<PreviewFileDiffConnection> {
        let raw = String::from_utf8_lossy(self.spec.diff.as_deref().unwrap_or_default());
        let mut files = parse(&raw);
        if let Some(paths) = paths.filter(|p| !p.is_empty()) {
            files.retain(|f| {
                [&f.old_path, &f.new_path]
                    .into_iter()
                    .flatten()
                    .any(|p| path_matches(p, &paths))
            });
        }
        let mut stat = DiffStat::default();
        for f in &files {
            stat.added = stat.added.saturating_add(to_int(i64::from(f.added)));
            stat.deleted = stat.deleted.saturating_add(to_int(i64::from(f.deleted)));
        }
        let base = match self.spec.base_rev.as_deref().filter(|r| !r.is_empty()) {
            Some(rev) => Some(Base {
                git: GitReader::new(services(ctx)?.repo_clients.clone(), &self.repo),
                repo: self.repo.clone(),
                rev: rev.to_owned(),
            }),
            None => None,
        };
        let total = files.len();
        let offset = decode_cursor(after.as_deref())?;
        let limit = first.map_or(i64::try_from(total).unwrap_or(i64::MAX), |n| {
            i64::from(n.max(0))
        });
        let skip = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        let fetched = total.saturating_sub(skip).min(take.saturating_add(1));
        Ok(PreviewFileDiffConnection {
            nodes: files
                .into_iter()
                .skip(skip)
                .take(take)
                .map(|f| PreviewFileDiff {
                    f,
                    base: base.clone(),
                })
                .collect(),
            total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
            page_info: PageInfo::new(offset, limit, fetched),
            diff_stat: stat,
            raw_diff: raw.into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_diffs() {
        let diff = "diff --git a/README.md b/README.md
index 1..2 100644
--- a/README.md
+++ b/README.md
@@ -1,3 +1,3 @@ # Title
 one
--- removed dashes
+++ added pluses
 three
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
\\ No newline at end of file
diff --git a/old.bin b/old.bin
deleted file mode 100644
Binary files a/old.bin and /dev/null differ
diff --git a/a.txt b/b.txt
similarity index 100%
rename from a.txt
rename to b.txt
";
        let files = parse(diff);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].old_path.as_deref(), Some("README.md"));
        assert_eq!((files[0].added, files[0].deleted), (1, 1));
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (1, 3, 1, 3)
        );
        assert_eq!(h.section, "# Title");
        assert_eq!(
            h.body,
            " one\n--- removed dashes\n+++ added pluses\n three\n"
        );
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].new_path.as_deref(), Some("new.txt"));
        assert_eq!(files[1].added, 1);
        assert!(
            files[1].hunks[0]
                .body
                .ends_with("\\ No newline at end of file\n")
        );
        assert!(files[2].binary && files[2].new_path.is_none());
        assert_eq!(
            (files[3].old_path.as_deref(), files[3].new_path.as_deref()),
            (Some("a.txt"), Some("b.txt"))
        );
    }

    fn patched(old: &str, diff: &str) -> String {
        let files = parse(diff);
        apply_hunks(old, &files[0].hunks).unwrap()
    }

    #[test]
    fn applies_hunks() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        // Changes in the middle and at the end, with untouched lines between and around.
        let diff = "--- a/f\n+++ b/f\n@@ -2,2 +2,3 @@\n 2\n-3\n+three\n+3.5\n@@ -9,2 +10,2 @@\n 9\n-10\n+ten\n";
        assert_eq!(
            patched(old, diff),
            "1\n2\nthree\n3.5\n4\n5\n6\n7\n8\n9\nten\n"
        );
        // Pure insertion after a line, and at the start.
        let diff = "--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+0\n@@ -3,0 +5,1 @@\n+3.5\n";
        assert_eq!(patched("1\n2\n3\n4\n", diff), "0\n1\n2\n3\n3.5\n4\n");
        // A new file without a final newline.
        let diff =
            "--- /dev/null\n+++ b/f\n@@ -0,0 +1,2 @@\n+a\n+b\n\\ No newline at end of file\n";
        assert_eq!(patched("", diff), "a\nb");
        // Adding the final newline.
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n";
        assert_eq!(patched("a\nb", diff), "a\nb\n");
        // Removing it.
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n+b\n\\ No newline at end of file\n";
        assert_eq!(patched("a\nb\n", diff), "a\nb");
        // A deleted file.
        let diff = "--- a/f\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        assert_eq!(patched("a\nb\n", diff), "");
        // A diff that does not fit the file.
        let files = parse("--- a/f\n+++ b/f\n@@ -5,1 +5,1 @@\n-x\n+y\n");
        assert!(apply_hunks("a\n", &files[0].hunks).is_err());
    }

    #[test]
    fn matches_paths() {
        let paths = vec!["src".to_owned(), "README.md".to_owned()];
        assert!(path_matches("src/a.rs", &paths));
        assert!(path_matches("README.md", &paths));
        assert!(!path_matches("srcx/a.rs", &paths));
        assert!(!path_matches("docs/README.md", &paths));
    }

    #[test]
    fn parses_plain_unified_diffs() {
        let files =
            parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- a/y\n+++ b/y\n@@ -2,0 +3 @@\n+c\n");
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].new_path.as_deref(), Some("y"));
        assert_eq!(files[1].hunks[0].new_start, 3);
    }
}
