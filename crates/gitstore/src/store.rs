//! [`GitStore`]: the repo node's repository manager.
//!
//! It owns the on-disk layout and serializes mutations per repo (clone,
//! fetch, janitor, delete), bounds clone and fetch concurrency separately
//! (`gitMaxConcurrentClones`), and caches opened repositories for reads.
//! It knows nothing about the database: callers pass the [`Remote`] and record
//! the returned ref updates themselves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gix::ObjectId;
use sourcegull_types::RepoId;
use tokio::sync::{OwnedMutexGuard, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::changes::{ChangeLog, ChangeLogs};
use crate::command::ProgressSink;
use crate::commitlog::{CommitLog, CommitLogs};
use crate::copy::{self, CopyHeader, CopySource};
use crate::error::{GitError, Result};
use crate::janitor::{self, Fragmentation, JanitorOutcome, JanitorTask};
use crate::layout::Layout;
use crate::patch::{ApplyOptions, ChangedFile, CreatedCommit, PatchCommit};
use crate::push::PushSpec;
use crate::refs::RefSnapshot;
use crate::remote::{Remote, write_askpass};
use crate::sync::{self, DEFAULT_REFSPECS, SyncOptions, SyncOutcome, blocking};

/// Memory for the indexes of loaded commit logs (about 12 bytes per commit).
const COMMIT_LOG_INDEX_BYTES: u64 = 256 * 1024 * 1024;
/// Memory for loaded changes files.
const CHANGE_LOG_BYTES: u64 = 256 * 1024 * 1024;

/// Configuration of a [`GitStore`].
#[derive(Debug, Clone)]
pub struct GitStoreConfig {
    /// Data directory (`/data`).
    pub root: PathBuf,
    /// The `git` executable.
    pub git_bin: PathBuf,
    /// Timeout of a clone.
    pub clone_timeout: Duration,
    /// Timeout of a fetch.
    pub fetch_timeout: Duration,
    /// Timeout of a janitor task.
    pub janitor_timeout: Duration,
    /// Timeout of a blame.
    pub blame_timeout: Duration,
    /// Timeout of an archive.
    pub archive_timeout: Duration,
    /// Concurrent clones (`gitMaxConcurrentClones`, default 5).
    pub max_concurrent_clones: usize,
    /// Concurrent fetches.
    pub max_concurrent_fetches: usize,
    /// Refspecs to fetch.
    pub refspecs: Vec<String>,
    /// Maintain commit-graph files after fetches.
    pub write_commit_graph: bool,
    /// Opened repositories kept in memory.
    pub repo_cache_capacity: u64,
    /// Default and maximum bytes returned by a file read.
    pub max_file_bytes: u64,
    /// Default and maximum entries of a tree listing.
    pub max_tree_entries: usize,
    /// Timeout of a commit from a patch (apply, write-tree, commit, update-ref).
    pub patch_timeout: Duration,
    /// Timeout of a push.
    pub push_timeout: Duration,
    /// Largest patch accepted by a commit from a patch.
    pub max_patch_bytes: usize,
}

impl GitStoreConfig {
    /// Defaults rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            git_bin: "git".into(),
            clone_timeout: Duration::from_secs(2 * 3600),
            fetch_timeout: Duration::from_secs(15 * 60),
            janitor_timeout: Duration::from_secs(2 * 3600),
            blame_timeout: Duration::from_secs(60),
            archive_timeout: Duration::from_secs(15 * 60),
            max_concurrent_clones: 5,
            max_concurrent_fetches: 10,
            refspecs: DEFAULT_REFSPECS.iter().map(|s| (*s).to_owned()).collect(),
            write_commit_graph: true,
            repo_cache_capacity: 2_000,
            max_file_bytes: 32 * 1024 * 1024,
            max_tree_entries: 100_000,
            patch_timeout: Duration::from_secs(5 * 60),
            push_timeout: Duration::from_secs(10 * 60),
            max_patch_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Optional behaviour of [`GitStore::sync_with`].
#[derive(Clone, Default)]
pub struct SyncHooks {
    /// Receives git progress lines (redacted).
    pub progress: Option<ProgressSink>,
    /// Kills the git process when cancelled.
    pub cancel: Option<CancellationToken>,
    /// Clone from scratch and swap the new copy in even if the repo is
    /// already cloned (e.g. after corruption).
    pub reclone: bool,
    /// What the code host reports about the project's repository; see
    /// [`SyncOptions::remote_hint`].
    pub remote_hint: crate::sync::RemoteHint,
}

impl std::fmt::Debug for SyncHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncHooks")
            .field("progress", &self.progress.is_some())
            .field("cancel", &self.cancel.is_some())
            .field("reclone", &self.reclone)
            .field("remote_hint", &self.remote_hint)
            .finish()
    }
}

impl SyncHooks {
    fn apply(&self, mut opts: SyncOptions) -> SyncOptions {
        if let Some(p) = &self.progress {
            opts.progress = Some(p.clone());
        }
        if let Some(c) = &self.cancel {
            opts.cancel = c.clone();
        }
        opts.remote_hint = self.remote_hint;
        opts
    }
}

/// On-disk status of a repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoStatus {
    /// Whether a complete clone exists.
    pub cloned: bool,
    /// Whether a first clone is running.
    pub clone_in_progress: bool,
    /// HEAD's symbolic target.
    pub head_ref: Option<String>,
    /// HEAD's commit.
    pub head_oid: Option<ObjectId>,
    /// Size in bytes (0 unless requested).
    pub size_bytes: u64,
    /// Object store fragmentation.
    pub fragmentation: Fragmentation,
    /// Repo directory.
    pub path: PathBuf,
}

/// Manages the bare repositories of one repo node.
pub struct GitStore {
    config: GitStoreConfig,
    layout: Layout,
    askpass: PathBuf,
    clone_permits: Arc<Semaphore>,
    fetch_permits: Arc<Semaphore>,
    locks: Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
    cloning: Mutex<HashSet<RepoId>>,
    repos: moka::sync::Cache<RepoId, gix::ThreadSafeRepository>,
    commit_logs: CommitLogs,
    change_logs: ChangeLogs,
}

impl std::fmt::Debug for GitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitStore")
            .field("root", &self.config.root)
            .finish_non_exhaustive()
    }
}

/// Clears the "cloning" mark when a clone ends, including by cancellation.
struct CloningGuard<'a> {
    store: &'a GitStore,
    repo: RepoId,
}

impl Drop for CloningGuard<'_> {
    fn drop(&mut self) {
        self.store
            .cloning
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.repo);
    }
}

impl GitStore {
    /// Opens the store: creates the directory layout and the askpass helper,
    /// and removes temp dirs left by a previous crash.
    pub fn open(config: GitStoreConfig) -> Result<Self> {
        let layout = Layout::new(&config.root);
        layout.create_dirs()?;
        let askpass = write_askpass(&layout.tmp_dir())?;
        let removed = janitor::remove_stale_tmp(&layout, config.clone_timeout)?;
        if removed > 0 {
            tracing::info!(removed, "removed stale temp dirs");
        }
        Ok(Self {
            clone_permits: Arc::new(Semaphore::new(config.max_concurrent_clones.max(1))),
            fetch_permits: Arc::new(Semaphore::new(config.max_concurrent_fetches.max(1))),
            locks: Mutex::new(HashMap::new()),
            cloning: Mutex::new(HashSet::new()),
            repos: moka::sync::Cache::new(config.repo_cache_capacity),
            commit_logs: CommitLogs::new(COMMIT_LOG_INDEX_BYTES),
            change_logs: ChangeLogs::new(CHANGE_LOG_BYTES).with_git(config.git_bin.clone()),
            layout,
            askpass,
            config,
        })
    }

    /// The configuration.
    pub fn config(&self) -> &GitStoreConfig {
        &self.config
    }

    /// The on-disk layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The bare repo directory of `repo`.
    pub fn repo_dir(&self, repo: RepoId) -> PathBuf {
        self.layout.repo_dir(repo)
    }

    /// Whether a complete clone of `repo` is on disk.
    pub fn is_cloned(&self, repo: RepoId) -> bool {
        self.repo_dir(repo).join("HEAD").is_file()
    }

    /// Whether a first clone of `repo` is running.
    pub fn is_cloning(&self, repo: RepoId) -> bool {
        self.cloning
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&repo)
    }

    /// Opens `repo` for reading (cached). Errors with
    /// [`GitError::CloneInProgress`] or [`GitError::RepoNotFound`] if it is
    /// not available.
    pub fn repo(&self, repo: RepoId) -> Result<gix::Repository> {
        let ts = match self.repos.get(&repo) {
            Some(ts) => ts,
            None => {
                if !self.is_cloned(repo) {
                    return Err(if self.is_cloning(repo) {
                        GitError::CloneInProgress(repo)
                    } else {
                        GitError::RepoNotFound(repo)
                    });
                }
                let ts = sync::open(&self.repo_dir(repo))?;
                self.repos.insert(repo, ts.clone());
                ts
            }
        };
        let mut local = ts.to_thread_local();
        local.object_cache_size_if_unset(4 * 1024 * 1024);
        Ok(local)
    }

    /// The commit metadata log of `repo`, or `None` until it has been
    /// built. Cheap when the log has not changed (one stat).
    pub fn commit_log(&self, repo: RepoId) -> Result<Option<Arc<CommitLog>>> {
        self.commit_logs.get(repo, &self.repo_dir(repo))
    }

    /// The commit log of `repo` with the files each of its commits changed,
    /// or `None` until the commit log has been built. Extends the
    /// changes file first when it lags behind the log, so call it from a
    /// blocking context.
    pub fn change_log(
        &self,
        repo: RepoId,
        git: &gix::Repository,
    ) -> Result<Option<(Arc<CommitLog>, Arc<ChangeLog>)>> {
        let Some(log) = self.commit_log(repo)? else {
            return Ok(None);
        };
        let changes = self.change_logs.current(repo, git, &log)?;
        Ok(Some((log, changes)))
    }

    /// Brings the changes file of `repo` up to its commit log in the
    /// background (after a clone or fetch), so reads rarely wait for it.
    fn warm_change_log(&self, repo: RepoId) {
        let (commit_logs, change_logs) = (self.commit_logs.clone(), self.change_logs.clone());
        let dir = self.repo_dir(repo);
        tokio::task::spawn_blocking(move || {
            let run = || -> Result<()> {
                let Some(log) = commit_logs.get(repo, &dir)? else {
                    return Ok(());
                };
                let git = sync::open(&dir)?.to_thread_local();
                change_logs.current(repo, &git, &log)?;
                Ok(())
            };
            if let Err(e) = run() {
                tracing::warn!(repo = repo.get(), error = %e, "changes log update failed");
            }
        });
    }

    /// Extends the commit log of `repo` after a fetch: when refs moved, or
    /// when there is no log yet (repos cloned before logs existed).
    async fn update_commit_log(&self, repo: RepoId, refs_moved: bool) {
        let logs = self.commit_logs.clone();
        let dir = self.repo_dir(repo);
        let _ = blocking(move || {
            let current = logs.get(repo, &dir).ok().flatten();
            if refs_moved || current.is_none() {
                crate::commitlog::update_logged(&dir, current.as_deref());
            }
            Ok(())
        })
        .await;
    }

    /// Runs `f` against `repo` on the blocking pool.
    pub async fn read<T, F>(self: &Arc<Self>, repo: RepoId, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&gix::Repository) -> Result<T> + Send + 'static,
    {
        let store = self.clone();
        blocking(move || {
            let r = store.repo(repo)?;
            f(&r)
        })
        .await
    }

    fn invalidate(&self, repo: RepoId) {
        self.repos.invalidate(&repo);
    }

    async fn lock(&self, repo: RepoId) -> OwnedMutexGuard<()> {
        let m = {
            let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
            if locks.len() > 1024 {
                locks.retain(|_, m| Arc::strong_count(m) > 1);
            }
            locks.entry(repo).or_default().clone()
        };
        m.lock_owned().await
    }

    /// Sync options for a clone or fetch with this store's settings.
    pub fn sync_options(&self, timeout: Duration) -> SyncOptions {
        SyncOptions {
            git_bin: self.config.git_bin.clone(),
            askpass: self.askpass.clone(),
            timeout,
            refspecs: self.config.refspecs.clone(),
            write_commit_graph: self.config.write_commit_graph,
            cancel: CancellationToken::new(),
            progress: None,
            remote_hint: crate::sync::RemoteHint::default(),
        }
    }

    /// Clones `repo` if it is not on disk. If it is and `fetch_if_cloned` is
    /// set, fetches instead. Returns `None` when nothing was done.
    pub async fn ensure(
        &self,
        repo: RepoId,
        remote: &Remote,
        fetch_if_cloned: bool,
    ) -> Result<Option<SyncOutcome>> {
        if self.is_cloned(repo) && !fetch_if_cloned {
            return Ok(None);
        }
        let _lock = self.lock(repo).await;
        if self.is_cloned(repo) {
            if !fetch_if_cloned {
                return Ok(None); // cloned by a concurrent caller
            }
            return self.fetch_locked(repo, remote).await.map(Some);
        }
        self.clone_locked(repo, remote).await.map(Some)
    }

    /// Clones `repo` from scratch, replacing an existing copy atomically. The
    /// old copy keeps serving reads until the new one is in place.
    pub async fn clone_repo(&self, repo: RepoId, remote: &Remote) -> Result<SyncOutcome> {
        let _lock = self.lock(repo).await;
        self.clone_locked(repo, remote).await
    }

    /// Fetches `repo`, cloning it first if it is missing.
    pub async fn fetch(&self, repo: RepoId, remote: &Remote) -> Result<SyncOutcome> {
        self.sync_with(repo, remote, SyncHooks::default()).await
    }

    /// Like [`fetch`](Self::fetch) (clone if missing, else fetch), or a forced
    /// re-clone with [`SyncHooks::reclone`], reporting git progress lines to
    /// `hooks.progress` and stopping when `hooks.cancel` fires. The repo role's
    /// update queue consumer uses it to publish clone progress.
    pub async fn sync_with(
        &self,
        repo: RepoId,
        remote: &Remote,
        hooks: SyncHooks,
    ) -> Result<SyncOutcome> {
        let _lock = self.lock(repo).await;
        if hooks.reclone || !self.is_cloned(repo) {
            return self.clone_locked_with(repo, remote, &hooks).await;
        }
        self.fetch_locked_with(repo, remote, &hooks).await
    }

    async fn clone_locked(&self, repo: RepoId, remote: &Remote) -> Result<SyncOutcome> {
        self.clone_locked_with(repo, remote, &SyncHooks::default())
            .await
    }

    async fn clone_locked_with(
        &self,
        repo: RepoId,
        remote: &Remote,
        hooks: &SyncHooks,
    ) -> Result<SyncOutcome> {
        let _guard = if self.is_cloned(repo) {
            None
        } else {
            self.cloning
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(repo);
            Some(CloningGuard { store: self, repo })
        };
        let _permit = self
            .clone_permits
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        let opts = hooks.apply(self.sync_options(self.config.clone_timeout));
        let result = sync::clone_repo(&self.layout, repo, remote, &opts).await;
        self.invalidate(repo);
        if result.is_ok() {
            self.change_logs.invalidate(repo);
            self.warm_change_log(repo);
        }
        result
    }

    async fn fetch_locked(&self, repo: RepoId, remote: &Remote) -> Result<SyncOutcome> {
        self.fetch_locked_with(repo, remote, &SyncHooks::default())
            .await
    }

    async fn fetch_locked_with(
        &self,
        repo: RepoId,
        remote: &Remote,
        hooks: &SyncHooks,
    ) -> Result<SyncOutcome> {
        let _permit = self
            .fetch_permits
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        let opts = hooks.apply(self.sync_options(self.config.fetch_timeout));
        let result = sync::fetch_repo(&self.repo_dir(repo), remote, &opts).await;
        self.invalidate(repo);
        if let Ok(outcome) = &result {
            self.update_commit_log(repo, !outcome.ref_updates.is_empty())
                .await;
            self.warm_change_log(repo);
        }
        result
    }

    /// Removes `repo` from disk. Returns whether it existed.
    pub async fn delete(&self, repo: RepoId) -> Result<bool> {
        let _lock = self.lock(repo).await;
        let dir = self.repo_dir(repo);
        self.invalidate(repo);
        self.commit_logs.invalidate(repo);
        self.change_logs.invalidate(repo);
        if !dir.exists() {
            return Ok(false);
        }
        // Move out of the way first so readers stop seeing it at once.
        let trash = tempfile::Builder::new()
            .prefix(&format!("delete-{}-", repo.get()))
            .tempdir_in(self.layout.tmp_dir())?;
        std::fs::rename(&dir, trash.path().join("repo.git"))?;
        blocking(move || Ok(trash.close()?)).await?;
        Ok(true)
    }

    /// The direct refs of `repo`, or `None` when it is not on disk.
    pub async fn ref_snapshot(&self, repo: RepoId) -> Result<Option<RefSnapshot>> {
        if !self.is_cloned(repo) {
            return Ok(None);
        }
        sync::snapshot_blocking(&self.repo_dir(repo))
            .await
            .map(Some)
    }

    /// The source side of a copy to another repo node (rebalancing): `repo`'s refs and
    /// a pack of the objects a target holding `haves` lacks. Haves this repo does not know are
    /// ignored. Fails with `RepoNotFound` when the repo is not cloned.
    pub async fn copy_out(
        self: &Arc<Self>,
        repo: RepoId,
        haves: Vec<ObjectId>,
    ) -> Result<CopySource> {
        let (refs, head_ref, haves) = self
            .read(repo, move |r| {
                let refs = crate::refs::snapshot(r)?;
                let (head_ref, _) = crate::refs::head(r)?;
                let haves: Vec<ObjectId> = haves.into_iter().filter(|h| r.has_object(h)).collect();
                Ok((refs, head_ref, haves))
            })
            .await?;
        let dir = self.repo_dir(repo);
        let origin_url = copy::origin_url(&self.config.git_bin, &dir).await;
        let have_set: HashSet<ObjectId> = haves.iter().copied().collect();
        let tips = copy::wanted_tips(&refs, &have_set);
        let pack = if tips.is_empty() {
            None
        } else {
            Some(copy::pack_stream(
                &self.config.git_bin,
                &dir,
                &tips,
                &haves,
                self.config.clone_timeout,
            )?)
        };
        Ok(CopySource {
            header: CopyHeader {
                refs,
                head_ref,
                origin_url,
            },
            pack,
        })
    }

    /// The target side of a copy: brings `repo` to `header`'s refs with the objects of the
    /// received `pack` file (in this store's temp directory, which it is moved out of). A repo
    /// not on disk is built in the temp directory and renamed into place like a clone; one on
    /// disk (an earlier round) is updated in place. The refs are verified against the header.
    pub async fn copy_in(
        &self,
        repo: RepoId,
        header: &CopyHeader,
        pack: Option<&Path>,
    ) -> Result<SyncOutcome> {
        let start = std::time::Instant::now();
        let _lock = self.lock(repo).await;
        let git = self.config.git_bin.clone();
        let timeout = self.config.clone_timeout;
        let dest = self.repo_dir(repo);
        let cloned = !self.is_cloned(repo);
        let (before, after) = if cloned {
            let work = tempfile::Builder::new()
                .prefix(&format!("copy-{}-", repo.get()))
                .tempdir_in(self.layout.tmp_dir())?;
            let tmp_repo = work.path().join("repo.git");
            copy::init_bare(&git, &tmp_repo, header.origin_url.as_deref()).await?;
            let after =
                copy::apply(&git, &tmp_repo, header, pack, &RefSnapshot::new(), timeout).await?;
            if self.config.write_commit_graph && !after.is_empty() {
                sync::write_commit_graph(&git, &tmp_repo, timeout).await?;
            }
            if !after.is_empty() {
                sync::update_commit_log(&tmp_repo).await;
            }
            sync::install(&tmp_repo, &dest, work.path())?;
            drop(work);
            (RefSnapshot::new(), after)
        } else {
            let before = sync::snapshot_blocking(&dest).await?;
            let after = copy::apply(&git, &dest, header, pack, &before, timeout).await?;
            (before, after)
        };
        let ref_updates = crate::refs::diff(&before, &after);
        self.invalidate(repo);
        if cloned {
            self.change_logs.invalidate(repo);
        } else {
            if self.config.write_commit_graph && !ref_updates.is_empty() {
                sync::write_commit_graph(&git, &dest, timeout).await?;
            }
            self.update_commit_log(repo, !ref_updates.is_empty()).await;
        }
        self.warm_change_log(repo);
        let (head_ref, head_oid) = sync::head_blocking(&dest).await?;
        Ok(SyncOutcome {
            ref_updates,
            head_ref,
            head_oid,
            size_bytes: sync::dir_size_blocking(&dest).await?,
            duration: start.elapsed(),
            progress_tail: String::new(),
            cloned,
            remote_unavailable: None,
        })
    }

    /// Reports the on-disk status of `repo`.
    pub async fn status(&self, repo: RepoId, include_size: bool) -> Result<RepoStatus> {
        let path = self.repo_dir(repo);
        let mut status = RepoStatus {
            cloned: self.is_cloned(repo),
            clone_in_progress: self.is_cloning(repo),
            path: path.clone(),
            ..Default::default()
        };
        if !status.cloned {
            return Ok(status);
        }
        let (head_ref, head_oid, fragmentation, size) = blocking(move || {
            let (head_ref, head_oid) = crate::refs::head(&sync::open(&path)?.to_thread_local())?;
            let frag = janitor::fragmentation(&path)?;
            let size = if include_size {
                sync::dir_size(&path)?
            } else {
                0
            };
            Ok((head_ref, head_oid, frag, size))
        })
        .await?;
        status.head_ref = head_ref;
        status.head_oid = head_oid;
        status.fragmentation = fragmentation;
        status.size_bytes = size;
        Ok(status)
    }

    /// Runs a janitor task on `repo` under its lock.
    pub async fn run_janitor(&self, repo: RepoId, task: JanitorTask) -> Result<JanitorOutcome> {
        let _lock = self.lock(repo).await;
        if !self.is_cloned(repo) {
            return Err(GitError::RepoNotFound(repo));
        }
        let out = janitor::run_task(
            &self.config.git_bin,
            &self.repo_dir(repo),
            task,
            self.config.janitor_timeout,
            CancellationToken::new(),
        )
        .await;
        self.invalidate(repo);
        out
    }

    /// Runs the task each repo needs per [`janitor::task_needed`]; repos
    /// found corrupt are returned so the caller can schedule a re-clone.
    pub async fn janitor_sweep(
        &self,
        thresholds: &janitor::MaintenanceThresholds,
    ) -> Result<Vec<RepoId>> {
        let repos = self.layout.list_repos()?;
        let mut corrupt = Vec::new();
        for (repo, path) in repos {
            let frag = match janitor::fragmentation(&path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(repo = %repo, error = %e, "reading fragmentation failed");
                    continue;
                }
            };
            let Some(task) = janitor::task_needed(&frag, thresholds) else {
                continue;
            };
            match self.run_janitor(repo, task).await {
                Ok(o) if o.corrupt => corrupt.push(repo),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(repo = %repo, ?task, error = %e, "janitor task failed");
                    // A failing gc often means corruption; confirm with fsck.
                    if let Ok(o) = self.run_janitor(repo, JanitorTask::Fsck).await {
                        if o.corrupt {
                            corrupt.push(repo);
                        }
                    }
                }
            }
        }
        janitor::remove_stale_tmp(&self.layout, self.config.clone_timeout)?;
        Ok(corrupt)
    }

    /// Applies a patch on top of a commit of `repo` and points a ref at the new
    /// commit ([`crate::patch`]). Runs without the repo lock: calls on one repo
    /// are independent of each other and of fetches. A re-clone or delete racing
    /// with it makes it fail, and the caller retries.
    pub async fn create_commit_from_patch(
        self: &Arc<Self>,
        repo: RepoId,
        commit: &PatchCommit,
    ) -> Result<CreatedCommit> {
        if commit.patch.len() > self.config.max_patch_bytes {
            return Err(GitError::TooLarge {
                what: "patch".into(),
                limit: self.config.max_patch_bytes as u64,
            });
        }
        crate::patch::validate(commit)?;
        let base = commit.base;
        self.read(repo, move |r| crate::read::find_commit(r, base).map(drop))
            .await?;
        let out = crate::patch::create_commit(
            &self.config.git_bin,
            &self.repo_dir(repo),
            &self.layout.tmp_dir(),
            commit,
            self.config.patch_timeout,
        )
        .await;
        self.invalidate(repo);
        out
    }

    /// Applies `patch` on top of `base` without committing ([`crate::patch::changed_files`])
    /// and returns the resulting tree and the changed files with their new contents. Fails with
    /// [`GitError::TooLarge`] when more than `max_files` files change (`None`: no limit) or
    /// their new contents add up to more than `max_content_bytes`.
    pub async fn changed_files(
        self: &Arc<Self>,
        repo: RepoId,
        base: ObjectId,
        patch: &bytes::Bytes,
        apply: &ApplyOptions,
        max_content_bytes: u64,
        max_files: Option<usize>,
    ) -> Result<(ObjectId, Vec<ChangedFile>)> {
        if patch.len() > self.config.max_patch_bytes {
            return Err(GitError::TooLarge {
                what: "patch".into(),
                limit: self.config.max_patch_bytes as u64,
            });
        }
        self.read(repo, move |r| crate::read::find_commit(r, base).map(drop))
            .await?;
        let (tree, raw) = crate::patch::changed_files(
            &self.config.git_bin,
            &self.repo_dir(repo),
            &self.layout.tmp_dir(),
            base,
            patch,
            apply,
            self.config.patch_timeout,
        )
        .await?;
        // The patch wrote new blobs: reopen so the cached repository sees them.
        self.invalidate(repo);
        if let Some(max) = max_files
            && raw.len() > max
        {
            return Err(GitError::TooLarge {
                what: format!("{} changed files", raw.len()),
                limit: max as u64,
            });
        }
        self.read(repo, move |r| {
            let mut total = 0u64;
            let mut files: Vec<ChangedFile> = raw.iter().map(ChangedFile::from_raw).collect();
            for (f, c) in files.iter().zip(&raw) {
                if f.needs_content() {
                    total += r.find_header(c.new_oid)?.size();
                }
            }
            if total > max_content_bytes {
                return Err(GitError::TooLarge {
                    what: format!("the changed files' contents ({total} bytes)"),
                    limit: max_content_bytes,
                });
            }
            for (f, c) in files.iter_mut().zip(&raw) {
                if f.needs_content() {
                    f.content = r.find_blob(c.new_oid)?.take_data();
                }
            }
            Ok((tree, files))
        })
        .await
    }

    /// Pushes a commit of `repo` to a remote ref ([`crate::push`]). Local remotes
    /// (paths, `file://`) are refused unless `allow_local`.
    pub async fn push(
        self: &Arc<Self>,
        repo: RepoId,
        spec: &PushSpec,
        allow_local: bool,
    ) -> Result<()> {
        let commit = spec.commit;
        self.read(repo, move |r| crate::read::find_commit(r, commit).map(drop))
            .await?;
        crate::push::push(
            &self.config.git_bin,
            &self.askpass,
            &self.repo_dir(repo),
            spec,
            allow_local,
            self.config.push_timeout,
        )
        .await
    }

    /// The `git` executable.
    pub fn git_bin(&self) -> &Path {
        &self.config.git_bin
    }
}
