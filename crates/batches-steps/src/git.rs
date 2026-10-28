//! Git operations on the host working copy:
//!
//! - changes: `git add --all`, then `git status --porcelain`, parsed by [`parse_git_status`];
//! - diff: `git add --all`, then `git diff --cached --no-prefix --binary` (unified diff without
//!   `a/`/`b/` prefixes, binary changes inlined);
//! - applying a cached diff: `git apply -p0 --binary` from stdin.
//!
//! Everything is relative to the commit checked out (`HEAD`), so diffs and changes are
//! cumulative over all steps. Git runs with `core.quotepath=false` (non-ASCII names are not
//! octal-escaped in changed file lists), without external diff drivers or colour.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::result::ChangedFiles;

/// A failed git command.
#[derive(Debug, thiserror::Error)]
#[error("git {command}: {message}")]
pub struct GitError {
    pub command: String,
    pub message: String,
}

async fn git(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, GitError> {
    let err = |message: String| GitError {
        command: args.join(" "),
        message,
    };
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| err(e.to_string()))?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().ok_or_else(|| err("no stdin".into()))?;
        pipe.write_all(input)
            .await
            .map_err(|e| err(e.to_string()))?;
        drop(pipe);
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| err(e.to_string()))?;
    if !out.status.success() {
        return Err(err(format!(
            "{} ({})",
            String::from_utf8_lossy(&out.stderr).trim(),
            out.status
        )));
    }
    Ok(out.stdout)
}

/// Stages everything and returns the changed files.
pub async fn changes(dir: &Path) -> Result<ChangedFiles, GitError> {
    git(dir, &["add", "--all"], None).await?;
    let out = git(dir, &["status", "--porcelain"], None).await?;
    parse_git_status(&String::from_utf8_lossy(&out)).map_err(|message| GitError {
        command: "status --porcelain".into(),
        message,
    })
}

/// Stages everything and returns the diff against `HEAD`.
pub async fn diff(dir: &Path) -> Result<Vec<u8>, GitError> {
    git(dir, &["add", "--all"], None).await?;
    git(
        dir,
        &[
            "diff",
            "--cached",
            "--no-prefix",
            "--binary",
            "--no-ext-diff",
            "--no-color",
        ],
        None,
    )
    .await
}

/// Applies a diff produced by [`diff`] to the working copy.
pub async fn apply(dir: &Path, diff: &[u8]) -> Result<(), GitError> {
    if diff.is_empty() {
        return Ok(());
    }
    git(dir, &["apply", "-p0", "--binary", "-"], Some(diff)).await?;
    Ok(())
}

/// Parses `git status --porcelain` output: `M`, `A`, `D` and `R`
/// in the index column; renames keep the new name.
pub fn parse_git_status(out: &str) -> Result<ChangedFiles, String> {
    let mut result = ChangedFiles::default();
    let stripped = out.trim();
    if stripped.is_empty() {
        return Ok(result);
    }
    for line in stripped.split('\n') {
        if line.len() < 4 || !line.is_char_boundary(3) {
            return Err(format!("git status line has unrecognized format: {line:?}"));
        }
        let file = &line[3..];
        match line.as_bytes()[0] {
            b'M' => result.modified.push(file.to_owned()),
            b'A' => result.added.push(file.to_owned()),
            b'D' => result.deleted.push(file.to_owned()),
            b'R' => {
                let new = file.rsplit(" -> ").next().unwrap_or(file);
                result.renamed.push(new.to_owned());
            }
            _ => {}
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status() {
        let c = parse_git_status(
            "M  README.md\nA  new.txt\nD  old.txt\nR  a.txt -> b.txt\n?? ignored\n",
        )
        .unwrap();
        assert_eq!(c.modified, ["README.md"]);
        assert_eq!(c.added, ["new.txt"]);
        assert_eq!(c.deleted, ["old.txt"]);
        assert_eq!(c.renamed, ["b.txt"]);
        assert!(parse_git_status("  \n").unwrap().is_empty());
        assert!(parse_git_status("M  a\nM\n").is_err());
    }
}
