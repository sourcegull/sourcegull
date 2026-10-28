//! Commits from patches (the reconciler's `CreateCommitFromPatch`).
//!
//! A patch is applied without a working copy: a temporary index file
//! (`GIT_INDEX_FILE` in the node's temp dir) is loaded with the base tree,
//! `git apply --cached` applies the patch to it (writing the new blobs into the
//! repo's object store), then `write-tree`, `commit-tree` and `update-ref` make
//! the commit and point the target ref at it. The repo's own index, HEAD and
//! other refs are never touched, and concurrent calls on one repo are
//! independent: each has its own index, object writes are safe in git, and
//! `update-ref` is atomic (the last writer of the same ref wins).
//!
//! Commands run with system and global git config ignored, so a node's
//! `~/.gitconfig` (signing, whitespace rules, hooks) cannot change the result.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use gix::ObjectId;

use crate::command::GitCommand;
use crate::error::{GitError, Result};

/// Author or committer of a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Display name.
    pub name: String,
    /// Email address.
    pub email: String,
}

/// The `git apply` options a caller may set (instead of raw arguments).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// `-p<n>`; `None` means 1 (the `a/` and `b/` prefixes of `git diff`).
    pub strip: Option<u32>,
    /// `--ignore-whitespace`.
    pub ignore_whitespace: bool,
    /// `--recount`.
    pub recount: bool,
    /// `--unidiff-zero`.
    pub unidiff_zero: bool,
    /// `--directory=<dir>` (repo-relative; empty for none).
    pub directory: String,
    /// `--allow-empty`: an empty patch commits the base tree unchanged.
    pub allow_empty: bool,
}

/// What [`create_commit`] commits.
#[derive(Debug, Clone)]
pub struct PatchCommit {
    /// The commit the patch applies to.
    pub base: ObjectId,
    /// Full ref name to point at the new commit (see [`normalize_ref`]).
    pub target_ref: String,
    /// `git diff` output (with `--binary` for binary files).
    pub patch: bytes::Bytes,
    /// Commit message.
    pub message: String,
    /// Author.
    pub author: Identity,
    /// Committer; `None` means the author.
    pub committer: Option<Identity>,
    /// Author and committer time (seconds since the epoch, UTC); `None` means now.
    pub date: Option<i64>,
    /// `git apply` options.
    pub apply: ApplyOptions,
}

/// A commit made by [`create_commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedCommit {
    /// The new commit.
    pub commit: ObjectId,
    /// Its tree.
    pub tree: ObjectId,
}

/// Returns `name` as a full ref name: names starting with `refs/` are kept,
/// anything else is a branch (`refs/heads/<name>`). Rejects names git would
/// not accept (`git check-ref-format` rules).
pub fn normalize_ref(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(GitError::InvalidArgument("ref name is required".into()));
    }
    let full = if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    };
    gix::refs::FullName::try_from(full.as_str())
        .map_err(|e| GitError::InvalidArgument(format!("invalid ref name {name:?}: {e}")))?;
    Ok(full)
}

fn check_identity(what: &str, id: &Identity) -> Result<()> {
    let bad = |s: &str| s.contains(['<', '>', '\n', '\r', '\0']);
    if id.name.trim().is_empty() || id.email.trim().is_empty() {
        return Err(GitError::InvalidArgument(format!(
            "{what} name and email are required"
        )));
    }
    if bad(&id.name) || bad(&id.email) {
        return Err(GitError::InvalidArgument(format!(
            "{what} name or email contains '<', '>' or a line break"
        )));
    }
    Ok(())
}

fn check_directory(dir: &str) -> Result<()> {
    if dir.is_empty() {
        return Ok(());
    }
    let ok = !dir.starts_with('/')
        && !dir.contains(['\\', '\0'])
        && dir.split('/').all(|c| c != ".." && c != ".");
    if ok {
        Ok(())
    } else {
        Err(GitError::InvalidArgument(format!(
            "invalid apply directory {dir:?}"
        )))
    }
}

/// Validates `c` before any git command runs.
pub fn validate(c: &PatchCommit) -> Result<()> {
    if normalize_ref(&c.target_ref)? != c.target_ref {
        return Err(GitError::InvalidArgument(format!(
            "target ref {:?} is not a full ref name",
            c.target_ref
        )));
    }
    if c.message.trim().is_empty() {
        return Err(GitError::InvalidArgument(
            "commit message is required".into(),
        ));
    }
    check_identity("author", &c.author)?;
    if let Some(committer) = &c.committer {
        check_identity("committer", committer)?;
    }
    check_directory(&c.apply.directory)?;
    Ok(())
}

/// Environment that keeps a node's system and global git config out of the
/// way (signing, `apply.whitespace`, hooks paths).
pub(crate) fn isolated_config_env() -> Vec<(String, OsString)> {
    vec![
        ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".into()),
        ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".into()),
    ]
}

fn apply_args(o: &ApplyOptions) -> Vec<String> {
    let mut args = vec![
        "--cached".to_owned(),
        "--whitespace=nowarn".to_owned(),
        format!("-p{}", o.strip.unwrap_or(1)),
    ];
    if o.ignore_whitespace {
        args.push("--ignore-whitespace".into());
    }
    if o.recount {
        args.push("--recount".into());
    }
    if o.unidiff_zero {
        args.push("--unidiff-zero".into());
    }
    if !o.directory.is_empty() {
        args.push(format!("--directory={}", o.directory.trim_end_matches('/')));
    }
    if o.allow_empty {
        args.push("--allow-empty".into());
    }
    args.push("-".into());
    args
}

fn parse_oid_output(op: &str, stdout: &[u8]) -> Result<ObjectId> {
    let s = String::from_utf8_lossy(stdout);
    ObjectId::from_hex(s.trim().as_bytes()).map_err(|_| GitError::Command {
        op: op.to_owned(),
        status: "unexpected output".into(),
        stderr: s.trim().chars().take(200).collect(),
    })
}

/// Applies `c.patch` to `c.base` in the bare repo at `repo_dir`, commits and
/// updates `c.target_ref`. The temp index lives under `tmp_dir`. The whole
/// sequence must finish within `timeout`. The base commit must exist (callers
/// check it, so a missing one is a typed error rather than a git failure).
pub async fn create_commit(
    git_bin: &Path,
    repo_dir: &Path,
    tmp_dir: &Path,
    c: &PatchCommit,
    timeout: Duration,
) -> Result<CreatedCommit> {
    validate(c)?;
    std::fs::create_dir_all(tmp_dir)?;
    let work = tempfile::Builder::new()
        .prefix("patch-")
        .tempdir_in(tmp_dir)?;
    let index = work.path().join("index");
    let run = create_commit_with_index(git_bin, repo_dir, &index, c, timeout);
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => Err(GitError::Timeout {
            op: "create commit from patch".into(),
            after: timeout,
        }),
    }
}

async fn create_commit_with_index(
    git_bin: &Path,
    repo_dir: &Path,
    index: &Path,
    c: &PatchCommit,
    timeout: Duration,
) -> Result<CreatedCommit> {
    let mut env = isolated_config_env();
    env.push(("GIT_INDEX_FILE".to_owned(), index.as_os_str().to_owned()));
    let git = |op: &str| {
        GitCommand::new(git_bin, op)
            .repo(repo_dir)
            .envs(env.clone())
            .timeout(timeout)
    };

    let tree = apply_to_index(&git, c.base, &c.patch, &c.apply).await?;

    let committer = c.committer.as_ref().unwrap_or(&c.author);
    let mut ident: Vec<(String, OsString)> = vec![
        ("GIT_AUTHOR_NAME".into(), c.author.name.clone().into()),
        ("GIT_AUTHOR_EMAIL".into(), c.author.email.clone().into()),
        ("GIT_COMMITTER_NAME".into(), committer.name.clone().into()),
        ("GIT_COMMITTER_EMAIL".into(), committer.email.clone().into()),
    ];
    if let Some(t) = c.date {
        let date = format!("{t} +0000");
        ident.push(("GIT_AUTHOR_DATE".into(), date.clone().into()));
        ident.push(("GIT_COMMITTER_DATE".into(), date.into()));
    }
    let mut message = c.message.clone();
    if !message.ends_with('\n') {
        message.push('\n');
    }
    let out = git("commit-tree")
        .config("commit.gpgsign=false")
        .envs(ident)
        .arg(tree.to_string())
        .args(["-p", &c.base.to_string(), "-F", "-"])
        .stdin(message.into())
        .run()
        .await?;
    let commit = parse_oid_output("commit-tree", &out.stdout)?;

    git("update-ref")
        .args(["-m", "sourcegull: commit from patch", "--"])
        .arg(&c.target_ref)
        .arg(commit.to_string())
        .run()
        .await?;
    Ok(CreatedCommit { commit, tree })
}

/// Loads `base`'s tree into the index `git` uses, applies `patch` to it and writes the
/// resulting tree.
async fn apply_to_index(
    git: &impl Fn(&str) -> GitCommand,
    base: ObjectId,
    patch: &bytes::Bytes,
    apply: &ApplyOptions,
) -> Result<ObjectId> {
    git("read-tree").arg(base.to_string()).run().await?;
    let applied = git("apply")
        .args(apply_args(apply))
        .stdin(patch.clone())
        .run()
        .await;
    match applied {
        Ok(_) => {}
        Err(GitError::Command { stderr, .. }) => return Err(GitError::PatchFailed { stderr }),
        Err(e) => return Err(e),
    }
    parse_oid_output("write-tree", &git("write-tree").run().await?.stdout)
}

/// One entry of `git diff-tree --raw` between a base and the tree a patch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    /// `A` (added), `M` (modified), `D` (deleted), `R` (renamed) or `T` (type changed).
    pub status: u8,
    /// The old mode (0 for `A`).
    pub old_mode: u32,
    /// The new mode (0 for `D`).
    pub new_mode: u32,
    /// The old blob (null for `A`).
    pub old_oid: ObjectId,
    /// The new blob (null for `D`).
    pub new_oid: ObjectId,
    /// The path (the new one for `R`).
    pub path: String,
    /// `R`: the old path.
    pub previous_path: Option<String>,
}

/// Applies `patch` to `base` in the bare repo at `repo_dir` without committing or touching
/// any ref, and returns the resulting tree with the changed files (renames detected).
/// The patch's blobs are written to the object store (unreferenced; `gc` prunes them).
pub async fn changed_files(
    git_bin: &Path,
    repo_dir: &Path,
    tmp_dir: &Path,
    base: ObjectId,
    patch: &bytes::Bytes,
    apply: &ApplyOptions,
    timeout: Duration,
) -> Result<(ObjectId, Vec<RawChange>)> {
    check_directory(&apply.directory)?;
    std::fs::create_dir_all(tmp_dir)?;
    let work = tempfile::Builder::new()
        .prefix("patch-")
        .tempdir_in(tmp_dir)?;
    let index = work.path().join("index");
    let mut env = isolated_config_env();
    env.push(("GIT_INDEX_FILE".to_owned(), index.as_os_str().to_owned()));
    let git = |op: &str| {
        GitCommand::new(git_bin, op)
            .repo(repo_dir)
            .envs(env.clone())
            .timeout(timeout)
    };
    let run = async {
        let tree = apply_to_index(&git, base, patch, apply).await?;
        let out = git("diff-tree")
            .args(["-r", "-z", "-M", "--raw", "--no-commit-id"])
            .arg(base.to_string())
            .arg(tree.to_string())
            .run()
            .await?;
        Ok((tree, parse_raw_diff(&out.stdout)?))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => Err(GitError::Timeout {
            op: "apply patch".into(),
            after: timeout,
        }),
    }
}

/// What happened to a [`ChangedFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Create,
    /// The content or mode (or type) changed.
    Update,
    Delete,
    /// Renamed from [`ChangedFile::previous_path`], maybe with content changes.
    Move,
}

/// The kind of a tree entry, from its mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Executable,
    Symlink,
    Submodule,
}

impl FileKind {
    /// The kind of a git mode (`0o100755` → executable).
    pub fn of_mode(mode: u32) -> Self {
        match mode {
            0o100755 => Self::Executable,
            0o120000 => Self::Symlink,
            0o160000 => Self::Submodule,
            _ => Self::Regular,
        }
    }
}

/// A file a patch changes, with its new content ([`GitStore::changed_files`]).
///
/// [`GitStore::changed_files`]: crate::GitStore::changed_files
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub action: FileAction,
    pub path: String,
    /// [`FileAction::Move`]: the old path.
    pub previous_path: Option<String>,
    /// The kind after the change (before it, for a delete).
    pub kind: FileKind,
    /// The kind before the change (`None` for a create).
    pub previous_kind: Option<FileKind>,
    /// Whether the content differs from the old file's (always for a create).
    pub content_changed: bool,
    /// The new content when it changed (a symlink's target; empty for submodules and
    /// deletes).
    pub content: Vec<u8>,
}

impl ChangedFile {
    /// A file from a raw change, without its content.
    pub fn from_raw(c: &RawChange) -> Self {
        let (action, content_changed) = match c.status {
            b'A' | b'C' => (FileAction::Create, true),
            b'D' => (FileAction::Delete, false),
            b'R' => (FileAction::Move, c.old_oid != c.new_oid),
            b'T' => (FileAction::Update, true),
            _ => (FileAction::Update, c.old_oid != c.new_oid),
        };
        let kind = FileKind::of_mode(if action == FileAction::Delete {
            c.old_mode
        } else {
            c.new_mode
        });
        Self {
            action,
            path: c.path.clone(),
            previous_path: c.previous_path.clone(),
            kind,
            previous_kind: (c.old_mode != 0).then(|| FileKind::of_mode(c.old_mode)),
            content_changed,
            content: Vec::new(),
        }
    }

    /// Whether the new content must be read (it changed and is a blob).
    pub fn needs_content(&self) -> bool {
        self.content_changed
            && self.action != FileAction::Delete
            && self.kind != FileKind::Submodule
    }
}

/// Parses `git diff-tree -r -z --raw` output:
/// `:<old mode> <new mode> <old oid> <new oid> <status>\0<path>\0[<new path>\0]`.
pub fn parse_raw_diff(out: &[u8]) -> Result<Vec<RawChange>> {
    let bad = |what: &str| GitError::Command {
        op: "diff-tree".into(),
        status: "unexpected output".into(),
        stderr: what.to_owned(),
    };
    let path = |p: &[u8]| {
        String::from_utf8(p.to_vec()).map_err(|_| {
            GitError::InvalidArgument(format!(
                "path {:?} is not UTF-8",
                String::from_utf8_lossy(p)
            ))
        })
    };
    let mut fields = out.split(|&b| b == 0);
    let mut changes = Vec::new();
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        let head = std::str::from_utf8(head).map_err(|_| bad("header"))?;
        let parts: Vec<&str> = head.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
            return Err(bad(head));
        };
        let mode = |m: &str| u32::from_str_radix(m, 8).map_err(|_| bad(head));
        let oid = |o: &str| ObjectId::from_hex(o.as_bytes()).map_err(|_| bad(head));
        let status = *status.as_bytes().first().ok_or_else(|| bad(head))?;
        let first = path(fields.next().ok_or_else(|| bad("missing path"))?)?;
        let (path, previous_path) = if matches!(status, b'R' | b'C') {
            let second = path(fields.next().ok_or_else(|| bad("missing path"))?)?;
            (second, Some(first))
        } else {
            (first, None)
        };
        changes.push(RawChange {
            status,
            old_mode: mode(old_mode)?,
            new_mode: mode(new_mode)?,
            old_oid: oid(old_oid)?,
            new_oid: oid(new_oid)?,
            path,
            previous_path,
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_are_normalized_and_checked() {
        assert_eq!(normalize_ref("feature/x").unwrap(), "refs/heads/feature/x");
        assert_eq!(
            normalize_ref("refs/heads/batch-changes/foo").unwrap(),
            "refs/heads/batch-changes/foo"
        );
        for bad in [
            "",
            "a..b",
            "refs/heads/x y",
            "refs/heads/x.lock",
            "x~1",
            "-x:y",
        ] {
            assert!(normalize_ref(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn apply_options_map_to_arguments() {
        let o = ApplyOptions {
            strip: Some(0),
            ignore_whitespace: true,
            recount: true,
            unidiff_zero: true,
            directory: "sub/dir/".into(),
            allow_empty: true,
        };
        assert_eq!(
            apply_args(&o),
            [
                "--cached",
                "--whitespace=nowarn",
                "-p0",
                "--ignore-whitespace",
                "--recount",
                "--unidiff-zero",
                "--directory=sub/dir",
                "--allow-empty",
                "-"
            ]
        );
        assert_eq!(
            apply_args(&ApplyOptions::default()),
            ["--cached", "--whitespace=nowarn", "-p1", "-"]
        );
        for bad in ["/abs", "a/../b", "./a", "a\\b"] {
            assert!(check_directory(bad).is_err(), "{bad:?}");
        }
        check_directory("a/b").unwrap();
    }

    #[test]
    fn identities_are_checked() {
        let id = |n: &str, e: &str| Identity {
            name: n.into(),
            email: e.into(),
        };
        check_identity("author", &id("Ann", "ann@example.com")).unwrap();
        assert!(check_identity("author", &id("", "a@b")).is_err());
        assert!(check_identity("author", &id("Ann", "")).is_err());
        assert!(check_identity("author", &id("Ann <x>", "a@b")).is_err());
        assert!(check_identity("author", &id("Ann", "a@b\nX: y")).is_err());
    }

    #[test]
    fn raw_diffs_are_parsed() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let z = "0".repeat(40);
        let out = format!(
            ":000000 100644 {z} {a} A\0new.txt\0:100644 000000 {a} {z} D\0gone\0\
             :100644 100755 {a} {b} R091\0old/x\0new/x\0:100644 100644 {a} {b} M\0m\0"
        );
        let c = parse_raw_diff(out.as_bytes()).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(
            (c[0].status, c[0].new_mode, c[0].path.as_str()),
            (b'A', 0o100644, "new.txt")
        );
        assert_eq!(
            (c[1].status, c[1].old_mode, c[1].new_mode),
            (b'D', 0o100644, 0)
        );
        assert_eq!(c[2].status, b'R');
        assert_eq!(c[2].path, "new/x");
        assert_eq!(c[2].previous_path.as_deref(), Some("old/x"));
        assert_eq!(c[2].new_mode, 0o100755);
        assert_eq!(c[3].new_oid.to_string(), b);
        assert!(parse_raw_diff(b"").unwrap().is_empty());
        assert!(parse_raw_diff(b":bad\0x\0").is_err());
    }
}
