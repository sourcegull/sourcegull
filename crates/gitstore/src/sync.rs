//! Clone and fetch through the `git` CLI.
//!
//! - A clone is built in `<root>/tmp` (same filesystem) and renamed into place
//!   only when complete, so readers never see a half-cloned repo. A re-clone
//!   swaps the new directory with the old one atomically
//!   (`renameat2(RENAME_EXCHANGE)`), then deletes the old copy.
//! - The URL and refspecs are passed on the command line on every fetch, so a
//!   changed remote URL needs no config rewrite and nothing secret is stored.
//! - Refs are snapshotted before and after; the difference is returned so the
//!   caller can record ref events.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gix::ObjectId;
use sourcegull_types::{RemoteUnavailable, RepoId};
use tokio_util::sync::CancellationToken;

use crate::command::{GitCommand, ProgressSink};
use crate::commitlog;
use crate::error::{GitError, Result};
use crate::layout::Layout;
use crate::refs::{self, RefSnapshot, RefUpdate};
use crate::remote::Remote;

/// Refspecs fetched by default: branches and tags. GitLab's
/// `refs/merge-requests/*`, `refs/keep-around/*` and `refs/pipelines/*` are
/// left out; add them to [`SyncOptions::refspecs`] if needed.
pub const DEFAULT_REFSPECS: &[&str] = &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Settings for one clone or fetch.
#[derive(Clone)]
pub struct SyncOptions {
    /// The `git` executable.
    pub git_bin: PathBuf,
    /// The askpass helper (see [`crate::remote::write_askpass`]).
    pub askpass: PathBuf,
    /// Timeout for the network operation.
    pub timeout: Duration,
    /// Refspecs to fetch.
    pub refspecs: Vec<String>,
    /// Write a commit-graph with changed-path Bloom filters after refs change.
    pub write_commit_graph: bool,
    /// Kills the git process when cancelled.
    pub cancel: CancellationToken,
    /// Receives git progress lines (redacted).
    pub progress: Option<ProgressSink>,
    /// What the code host's listing said about the project's repository (see
    /// [`GitError::remote_unavailable`]).
    pub remote_hint: RemoteHint,
}

impl SyncOptions {
    /// Options with defaults: `git` from `PATH`, 1 hour timeout, branches and tags.
    pub fn new(askpass: impl Into<PathBuf>) -> Self {
        Self {
            git_bin: "git".into(),
            askpass: askpass.into(),
            timeout: Duration::from_secs(3600),
            refspecs: DEFAULT_REFSPECS.iter().map(|s| (*s).to_owned()).collect(),
            write_commit_graph: true,
            cancel: CancellationToken::new(),
            progress: None,
            remote_hint: RemoteHint::default(),
        }
    }

    fn git(&self, op: &str, remote: &Remote) -> GitCommand {
        GitCommand::new(&self.git_bin, op)
            .timeout(self.timeout)
            .cancel_on(self.cancel.clone())
            .redactor(remote.redactor())
            .progress(self.progress.clone())
            .envs(remote.git_env(&self.askpass))
            // Ignore credential helpers from system/global config: credentials
            // come only from the askpass helper.
            .config("credential.helper=")
    }
}

/// Result of a clone or fetch.
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Changed refs. On a first clone every ref is reported as created; on a
    /// re-clone the changes are relative to the replaced copy.
    pub ref_updates: Vec<RefUpdate>,
    /// HEAD's symbolic target after the operation.
    pub head_ref: Option<String>,
    /// HEAD's commit after the operation.
    pub head_oid: Option<ObjectId>,
    /// Size of the repo directory in bytes.
    pub size_bytes: u64,
    /// Wall time of the operation.
    pub duration: Duration,
    /// Last lines of git's progress output, redacted.
    pub progress_tail: String,
    /// Whether this was a clone.
    pub cloned: bool,
    /// The remote has no git repository the node can fetch: never created, disabled, or the
    /// token may not download code. The local copy is an empty bare repo (or the previous
    /// copy, on a fetch); a later fetch fills it once the repository becomes available.
    pub remote_unavailable: Option<RemoteUnavailable>,
}

/// What the code host's listing says about a project's repository, used to classify a failed
/// fetch (see [`GitError::remote_unavailable`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteHint {
    /// The project has no repository yet (GitLab `empty_repo`).
    pub empty: bool,
    /// The project's repository feature is disabled (GitLab `repository_access_level`).
    pub repository_disabled: bool,
    /// The token's access to the private project is below what reading code needs (GitLab:
    /// below Reporter).
    pub below_reporter: bool,
}

/// Opens a bare repo for reading, ignoring system and global git config so
/// reads behave the same on every node.
pub fn open(path: &Path) -> Result<gix::ThreadSafeRepository> {
    gix::open_opts(path, gix::open::Options::isolated())
        .map(gix::Repository::into_sync)
        .map_err(GitError::gix)
}

/// Clones `remote` for `repo` and moves it into place, replacing an existing copy.
pub async fn clone_repo(
    layout: &Layout,
    repo: RepoId,
    remote: &Remote,
    opts: &SyncOptions,
) -> Result<SyncOutcome> {
    let start = Instant::now();
    std::fs::create_dir_all(layout.tmp_dir())?;
    let work = tempfile::Builder::new()
        .prefix(&format!("clone-{}-", repo.get()))
        .tempdir_in(layout.tmp_dir())?;
    let tmp_repo = work.path().join("repo.git");

    GitCommand::new(&opts.git_bin, "init")
        .args(["--bare", "--quiet"])
        .arg(&tmp_repo)
        .timeout(Duration::from_secs(60))
        .run()
        .await?;
    // Local settings: no automatic gc (the janitor schedules maintenance), keep
    // the (credential-free) origin URL for admins debugging on the node.
    for (k, v) in [
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("core.logAllRefUpdates", "false"),
        ("remote.origin.url", remote.url()),
    ] {
        GitCommand::new(&opts.git_bin, "config")
            .repo(&tmp_repo)
            .args([k, v])
            .timeout(Duration::from_secs(60))
            .run()
            .await?;
    }

    let remote_head = detect_remote_head(remote, opts).await;
    let (progress_tail, remote_unavailable) = match run_fetch(&tmp_repo, remote, opts).await {
        Ok(tail) => (tail, None),
        Err(e) => match e.remote_unavailable(&opts.remote_hint) {
            Some(why) => (String::new(), Some(why)),
            None => return Err(e),
        },
    };
    let after = snapshot_blocking(&tmp_repo).await?;
    set_head(&opts.git_bin, &tmp_repo, remote_head.as_deref(), &after).await?;
    if opts.write_commit_graph && !after.is_empty() {
        write_commit_graph(&opts.git_bin, &tmp_repo, opts.timeout).await?;
    }
    if !after.is_empty() {
        update_commit_log(&tmp_repo).await;
    }

    let dest = layout.repo_dir(repo);
    let before = if dest.exists() {
        snapshot_blocking(&dest).await.unwrap_or_default()
    } else {
        RefSnapshot::new()
    };
    install(&tmp_repo, &dest, work.path())?;
    drop(work); // removes the temp dir, now holding the replaced copy if any

    let (head_ref, head_oid) = head_blocking(&dest).await?;
    Ok(SyncOutcome {
        ref_updates: refs::diff(&before, &after),
        head_ref,
        head_oid,
        size_bytes: dir_size_blocking(&dest).await?,
        duration: start.elapsed(),
        progress_tail,
        cloned: true,
        remote_unavailable,
    })
}

/// Fetches into the existing bare repo at `path`.
pub async fn fetch_repo(path: &Path, remote: &Remote, opts: &SyncOptions) -> Result<SyncOutcome> {
    let start = Instant::now();
    let before = snapshot_blocking(path).await?;
    let (progress_tail, remote_unavailable) = match run_fetch(path, remote, opts).await {
        Ok(tail) => (tail, None),
        // Still no repository the node can fetch: nothing to fetch, keep the local copy.
        Err(e) => match e.remote_unavailable(&opts.remote_hint) {
            Some(why) => (String::new(), Some(why)),
            None => return Err(e),
        },
    };
    let after = snapshot_blocking(path).await?;
    let ref_updates = refs::diff(&before, &after);

    let (mut head_ref, mut head_oid) = head_blocking(path).await?;
    if head_oid.is_none() && !after.is_empty() {
        // HEAD's branch is gone (default branch renamed or first push to an
        // empty repo): ask the remote again.
        let remote_head = detect_remote_head(remote, opts).await;
        set_head(&opts.git_bin, path, remote_head.as_deref(), &after).await?;
        (head_ref, head_oid) = head_blocking(path).await?;
    }
    if opts.write_commit_graph && !ref_updates.is_empty() {
        write_commit_graph(&opts.git_bin, path, opts.timeout).await?;
    }
    Ok(SyncOutcome {
        ref_updates,
        head_ref,
        head_oid,
        size_bytes: dir_size_blocking(path).await?,
        duration: start.elapsed(),
        progress_tail,
        cloned: false,
        remote_unavailable,
    })
}

async fn run_fetch(path: &Path, remote: &Remote, opts: &SyncOptions) -> Result<String> {
    let out = opts
        .git("fetch", remote)
        .repo(path)
        .args([
            "--prune",
            "--progress",
            "--no-tags",
            "--no-write-fetch-head",
        ])
        .arg("--")
        .arg(remote.url())
        .args(&opts.refspecs)
        .run()
        .await?;
    Ok(out.stderr_tail)
}

/// Asks the remote for its HEAD symref (`git ls-remote --symref <url> HEAD`).
/// Failures are not fatal: a fallback branch is chosen instead.
async fn detect_remote_head(remote: &Remote, opts: &SyncOptions) -> Option<String> {
    let out = opts
        .git("ls-remote", remote)
        .args(["--symref", "--"])
        .arg(remote.url())
        .arg("HEAD")
        .progress(None)
        .timeout(opts.timeout.min(Duration::from_secs(120)))
        .run()
        .await;
    match out {
        Ok(out) => parse_symref(&String::from_utf8_lossy(&out.stdout)),
        Err(e) => {
            tracing::debug!(error = %e, "ls-remote --symref failed; guessing HEAD");
            None
        }
    }
}

fn parse_symref(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let rest = l.strip_prefix("ref: ")?;
        let (target, name) = rest.split_once('\t')?;
        (name == "HEAD").then(|| target.to_owned())
    })
}

/// Points HEAD at `preferred` if it exists, else at `main`, `master` or the
/// first branch. Leaves HEAD alone when there are no branches.
pub async fn set_head(
    git_bin: &Path,
    path: &Path,
    preferred: Option<&str>,
    refs: &RefSnapshot,
) -> Result<()> {
    let Some(target) = choose_head(preferred, refs) else {
        return Ok(());
    };
    GitCommand::new(git_bin, "symbolic-ref")
        .repo(path)
        .args(["HEAD", target])
        .timeout(Duration::from_secs(60))
        .run()
        .await?;
    Ok(())
}

fn choose_head<'a>(preferred: Option<&'a str>, refs: &'a RefSnapshot) -> Option<&'a str> {
    if let Some(p) = preferred.filter(|p| refs.contains_key(*p)) {
        return Some(p);
    }
    ["refs/heads/main", "refs/heads/master"]
        .into_iter()
        .find(|r| refs.contains_key(*r))
        .or_else(|| {
            refs.keys()
                .find(|k| k.starts_with("refs/heads/"))
                .map(String::as_str)
        })
}

/// Writes or extends the commit-graph with changed-path Bloom filters.
pub async fn write_commit_graph(git_bin: &Path, path: &Path, timeout: Duration) -> Result<()> {
    GitCommand::new(git_bin, "commit-graph")
        .repo(path)
        .args([
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
            "--no-progress",
        ])
        .timeout(timeout)
        .run()
        .await?;
    Ok(())
}

/// Moves the finished clone at `tmp_repo` to `dest`. If `dest` exists the two
/// are exchanged atomically and the old copy ends up under `trash_dir`.
pub(crate) fn install(tmp_repo: &Path, dest: &Path, trash_dir: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !dest.exists() {
        std::fs::rename(tmp_repo, dest)?;
        return Ok(());
    }
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, tmp_repo, CWD, dest, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Filesystem without RENAME_EXCHANGE: two renames, with a short
            // window in which the repo is missing.
            let old = trash_dir.join("old.git");
            std::fs::rename(dest, &old)?;
            std::fs::rename(tmp_repo, dest)?;
            Ok(())
        }
    }
}

/// Extends the commit log on the blocking pool; failures are logged.
pub(crate) async fn update_commit_log(path: &Path) {
    let path = path.to_owned();
    let _ = tokio::task::spawn_blocking(move || commitlog::update_logged(&path, None)).await;
}

pub(crate) async fn snapshot_blocking(path: &Path) -> Result<RefSnapshot> {
    let path = path.to_owned();
    blocking(move || refs::snapshot(&open(&path)?.to_thread_local())).await
}

pub(crate) async fn head_blocking(path: &Path) -> Result<(Option<String>, Option<ObjectId>)> {
    let path = path.to_owned();
    blocking(move || refs::head(&open(&path)?.to_thread_local())).await
}

pub(crate) async fn dir_size_blocking(path: &Path) -> Result<u64> {
    let path = path.to_owned();
    blocking(move || Ok(dir_size(&path)?)).await
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| GitError::Io(std::io::Error::other(e)))?
}

/// Total size in bytes of the files under `path` (symlinks are not followed).
pub fn dir_size(path: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Files can disappear under a concurrent gc.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symref_output() {
        let out = "ref: refs/heads/trunk\tHEAD\n0123456789012345678901234567890123456789\tHEAD\n";
        assert_eq!(parse_symref(out).as_deref(), Some("refs/heads/trunk"));
        assert_eq!(parse_symref("abc\tHEAD\n"), None);
    }

    #[test]
    fn chooses_a_sensible_head() {
        let oid = ObjectId::null(gix::hash::Kind::Sha1);
        let refs: RefSnapshot = [
            ("refs/heads/dev".to_owned(), oid),
            ("refs/heads/master".to_owned(), oid),
            ("refs/tags/v1".to_owned(), oid),
        ]
        .into();
        assert_eq!(
            choose_head(Some("refs/heads/dev"), &refs),
            Some("refs/heads/dev")
        );
        assert_eq!(
            choose_head(Some("refs/heads/gone"), &refs),
            Some("refs/heads/master")
        );
        assert_eq!(choose_head(None, &RefSnapshot::new()), None);
        let only_dev: RefSnapshot = [("refs/heads/dev".to_owned(), oid)].into();
        assert_eq!(choose_head(None, &only_dev), Some("refs/heads/dev"));
    }
}
