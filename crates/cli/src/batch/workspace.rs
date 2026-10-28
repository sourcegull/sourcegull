//! Working copies for local runs: the repository's tree at the workspace's
//! commit is downloaded as an archive from the instance (`/{repo}@{commit}/-/raw/…`), unpacked
//! into a temporary directory and committed into a fresh git repository (`git init`,
//! `git add --force --all`, `git commit`), so the step runner's `git diff` against `HEAD` is
//! the diff against the base commit. With `onlyFetchWorkspace`, only the workspace directory
//! is downloaded, plus the root `.gitignore` and `.gitattributes` when present (not those of
//! the directories in between).

use std::path::Path;
use std::process::Stdio;

use super::client::{Client, archive_path, raw_file_path};

/// Author and committer of the base commit.
const GIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "gull"),
    ("GIT_AUTHOR_EMAIL", "gull@sourcegull.invalid"),
    ("GIT_COMMITTER_NAME", "gull"),
    ("GIT_COMMITTER_EMAIL", "gull@sourcegull.invalid"),
];

/// Where to check out.
#[derive(Debug, Clone)]
pub struct Checkout<'a> {
    pub repo: &'a str,
    pub commit: &'a str,
    /// The workspace directory ('' for the root).
    pub path: &'a str,
    pub only_fetch_workspace: bool,
}

/// Fills the empty directory `dir` with a working copy of `checkout`.
pub async fn prepare(client: &Client, checkout: &Checkout<'_>, dir: &Path) -> anyhow::Result<()> {
    let sub = if checkout.only_fetch_workspace {
        checkout.path
    } else {
        ""
    };
    let archive = client
        .get_bytes(
            &archive_path(checkout.repo, checkout.commit, sub),
            "application/x-tar",
        )
        .await
        .map_err(|e| anyhow::anyhow!("downloading {}@{}: {e}", checkout.repo, checkout.commit))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "downloading {}@{}: not found",
                checkout.repo,
                checkout.commit
            )
        })?;
    let target = dir.to_owned();
    tokio::task::spawn_blocking(move || unpack(&archive, &target)).await??;
    if !sub.is_empty() {
        for name in [".gitignore", ".gitattributes"] {
            if dir.join(name).exists() {
                continue;
            }
            let file = client
                .get_bytes(
                    &raw_file_path(checkout.repo, checkout.commit, name),
                    "application/octet-stream",
                )
                .await
                .map_err(|e| anyhow::anyhow!("downloading {name}: {e}"))?;
            if let Some(content) = file {
                tokio::fs::write(dir.join(name), content).await?;
            }
        }
        // The workspace directory exists even when the archive had nothing in it.
        tokio::fs::create_dir_all(dir.join(sub)).await?;
    }
    init_git(dir).await
}

/// Unpacks a tar archive into `dir`; entries that would land outside it are refused.
pub fn unpack(archive: &[u8], dir: &Path) -> anyhow::Result<()> {
    let mut ar = tar::Archive::new(archive);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    for entry in ar.entries()? {
        let mut entry = entry?;
        // `pax_global_header` and similar metadata entries carry no file.
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        if !entry.unpack_in(dir)? {
            anyhow::bail!(
                "archive entry {:?} is outside the working copy",
                entry.path()?.display()
            );
        }
    }
    Ok(())
}

/// Commits the directory's files as the base commit of the working copy.
pub async fn init_git(dir: &Path) -> anyhow::Result<()> {
    git(dir, &["init", "--quiet"]).await?;
    git(dir, &["add", "--force", "--all"]).await?;
    git(
        dir,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "--no-gpg-sign",
            "-m",
            "gull bulk: base",
        ],
    )
    .await
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .envs(GIT_IDENTITY)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running git: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            // `set_path` refuses `..`; write the raw name as a hostile archive would.
            let name = h.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn unpack_refuses_escapes() {
        let dir = tempfile::tempdir().unwrap();
        unpack(&tar_of(&[("a/b.txt", b"hi")]), dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("a/b.txt")).unwrap(), b"hi");
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        assert!(unpack(&tar_of(&[("../evil.txt", b"x")]), &inner).is_err());
        assert!(!dir.path().join("evil.txt").exists());
    }

    #[tokio::test]
    async fn base_commit_holds_every_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.path().join("kept.log"), "x").unwrap();
        init_git(dir.path()).await.unwrap();
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            out.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let out = std::process::Command::new("git")
            .args(["ls-files"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            ".gitignore\nkept.log\n"
        );
    }
}
