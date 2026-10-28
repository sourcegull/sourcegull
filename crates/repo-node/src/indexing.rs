//! The repo node's search index: one [`ShardIndex`] under the data directory and the queue that
//! keeps it current.
//!
//! Fetches that move refs queue the repo on the debounce's leading edge: a repo with no
//! build started within the debounce window (5 s by default) is due at once, and pushes that
//! arrive within the window of a build wait for its end, so a burst becomes one trailing
//! build. First clones are queued at once. A repo already queued keeps its earlier due time,
//! so steady pushes cannot postpone a build forever, and a push while a build runs queues
//! the repo again, so the last push is always indexed. A reconcile pass at start and then
//! periodically queues every repo on disk (a cheap check when nothing moved) and drops shards
//! of repos no longer on disk.
//!
//! Each build indexes the default branch as `HEAD` plus the repo's `search.index.branches`, and is
//! skipped when the index already holds those branches at those commits, the repo's static ranks
//! at the generation `ranking_path_ranks` has, and the content of large files as
//! `search.index.alwaysIndex` asks (a changed rule rebuilds the shard at the next reconcile pass). A new generation (found by the reconcile pass)
//! rebuilds the repo's shard with the new ranks. Once the builds a reconcile pass queued are done,
//! small repos are packed into compound shards, so merges run at most once per reconcile interval.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use sourcegull_db::{IndexedBranchRow, IndexedState};
use sourcegull_search_exec::index::{BuildRequest, SearchIndex};
use sourcegull_search_exec::{
    IndexControl, IndexUpdate, MergeReport, ShardIndex, branches_to_index, update_index,
};
use sourcegull_types::RepoId;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use sourcegull_gitstore::GitStore;
use sourcegull_ranking::store::RankingStore;
use sourcegull_types::metric_names;

use super::RepoNode;

/// Repos waiting to be indexed, each with the time it is due, the ones a site admin asked
/// to rebuild in full, and when each repo's latest build started (for the debounce).
#[derive(Debug, Default)]
pub(crate) struct IndexQueue {
    state: Mutex<QueueState>,
    wake: Notify,
    /// The debounce window of [`IndexQueue::push_debounced`].
    window: Duration,
}

#[derive(Debug, Default)]
struct QueueState {
    due: HashMap<RepoId, Instant>,
    forced: HashSet<RepoId>,
    /// Repos being indexed now, with the time the update started.
    running: HashMap<RepoId, Instant>,
    /// Start times of builds within the last window (older ones are pruned).
    built: HashMap<RepoId, Instant>,
}

impl QueueState {
    /// The earliest time a debounced push of `repo` may build: the end of the window of the
    /// latest build, or of the update running now.
    fn debounced_at(&self, repo: RepoId, window: Duration, now: Instant) -> Instant {
        let last = match (self.running.get(&repo), self.built.get(&repo)) {
            (Some(a), Some(b)) => Some(*a.max(b)),
            (a, b) => a.or(b).copied(),
        };
        last.map_or(now, |t| (t + window).max(now))
    }
}

impl IndexQueue {
    /// An empty queue whose debounced pushes use `window`.
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `repo` after a fetch that moved refs. On the leading edge (no build of the repo
    /// started within the window, none running) it is due now; otherwise at the end of the
    /// window, so a burst of pushes becomes one trailing build. An earlier due time is kept.
    pub(crate) fn push_debounced(&self, repo: RepoId) {
        let now = Instant::now();
        let mut state = self.lock();
        let at = state.debounced_at(repo, self.window, now);
        let entry = state.due.entry(repo).or_insert(at);
        if at < *entry {
            *entry = at;
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Marks the start of an index update of `repo` (taken from the queue).
    fn start(&self, repo: RepoId) {
        self.lock().running.insert(repo, Instant::now());
    }

    /// Marks the end of the update [`IndexQueue::start`] began. `built` says whether it built
    /// (or tried to build) a shard; only those count towards the debounce window, so a
    /// reconcile check that found nothing to do does not delay the next push. A push queued
    /// while an update that did not build was running is moved up to its debounced time.
    fn finish(&self, repo: RepoId, built: bool) {
        let now = Instant::now();
        let window = self.window;
        let mut state = self.lock();
        let started = state.running.remove(&repo);
        if built && let Some(t) = started {
            state.built.insert(repo, t);
        }
        state.built.retain(|_, t| *t + window > now);
        if !built && state.due.contains_key(&repo) {
            let at = state.debounced_at(repo, window, now);
            if let Some(due) = state.due.get_mut(&repo)
                && at < *due
            {
                *due = at;
            }
        }
    }

    /// Queues `repo` to be indexed after `delay`, or keeps its earlier due time.
    pub(crate) fn push(&self, repo: RepoId, delay: Duration) {
        let at = Instant::now() + delay;
        let mut state = self.lock();
        let entry = state.due.entry(repo).or_insert(at);
        if at < *entry {
            *entry = at;
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Queues a full rebuild of `repo`, due now.
    pub(crate) fn push_forced(&self, repo: RepoId) {
        self.lock().forced.insert(repo);
        self.push(repo, Duration::ZERO);
    }

    /// Whether `repo` is queued.
    pub(crate) fn contains(&self, repo: RepoId) -> bool {
        self.lock().due.contains_key(&repo)
    }

    /// Takes the repos that are due (oldest first, each with whether a full rebuild was asked
    /// for) and returns when the next one is due.
    fn take_due(&self) -> (Vec<(RepoId, bool)>, Option<Instant>) {
        let now = Instant::now();
        let mut state = self.lock();
        let mut ready: Vec<(Instant, RepoId)> = state
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(id, at)| (*at, *id))
            .collect();
        ready.sort_unstable();
        let mut out = Vec::with_capacity(ready.len());
        for (_, id) in ready {
            state.due.remove(&id);
            let forced = state.forced.remove(&id);
            out.push((id, forced));
        }
        let next = state.due.values().min().copied();
        (out, next)
    }

    pub(crate) fn len(&self) -> usize {
        self.lock().due.len()
    }
}

/// The node's search index, as a trait object and as the concrete shards.
type OpenedIndex = (Option<Arc<dyn SearchIndex>>, Option<Arc<ShardIndex>>);

/// Opens the shards under the store's index directory. Shards that fail to open are deleted
/// and rebuilt by the first reconcile pass.
pub(crate) fn open_index(store: &GitStore) -> anyhow::Result<OpenedIndex> {
    let dir = store.layout().index_dir();
    let (index, failed) = ShardIndex::open(&dir, Default::default())
        .with_context(|| format!("opening the search index in {}", dir.display()))?;
    for (path, e) in failed {
        tracing::warn!(path = %path.display(), error = %e, "dropping an unreadable index shard");
        let _ = std::fs::remove_file(&path);
    }
    let index = Arc::new(index);
    Ok((Some(index.clone()), Some(index)))
}

impl RepoNode {
    /// The search index, when this node has one.
    pub fn index(&self) -> Option<&Arc<dyn SearchIndex>> {
        self.inner.index.as_ref()
    }

    /// Queues `repo` for indexing after `delay` (no-op without an index).
    pub(crate) fn queue_index(&self, repo: RepoId, delay: Duration) {
        if self.inner.index.is_some() {
            self.inner.index_queue.push(repo, delay);
        }
    }

    /// Queues `repo` for indexing after a fetch that moved refs, debounced (no-op without an
    /// index): see [`IndexQueue::push_debounced`].
    pub(crate) fn queue_index_debounced(&self, repo: RepoId) {
        if self.inner.index.is_some() {
            self.inner.index_queue.push_debounced(repo);
        }
    }

    /// Brings the index up to date for one repo: builds it when its branch heads moved or its
    /// ranks changed (in full when `force` is set, as a site admin's reindex asks), drops it
    /// when the repo is gone from disk, deleted or blocked.
    pub async fn index_repo(&self, id: RepoId, force: bool) -> anyhow::Result<IndexUpdate> {
        let inner = &*self.inner;
        let index = inner
            .index
            .clone()
            .context("this node has no search index")?;
        let repo = inner.db.repos().get(id).await?;
        let live = repo
            .as_ref()
            .filter(|r| r.blocked.is_none() && inner.store.is_cloned(id));
        let Some(repo) = live else {
            let removed = tokio::task::spawn_blocking(move || index.remove(id)).await??;
            return Ok(if removed {
                IndexUpdate::Removed
            } else {
                IndexUpdate::Unchanged
            });
        };
        let (extra, ranked, always_index) = {
            let site = inner.site.load();
            let extra = site
                .value
                .search_index_branches
                .get(&repo.name)
                .cloned()
                .unwrap_or_default();
            (
                extra,
                site.value.codeintel_ranking_enabled,
                site.value.always_index(&repo.name),
            )
        };
        let generation = if ranked {
            RankingStore::new(&inner.db).generation(id).await?
        } else {
            None
        };
        let store = inner.store.clone();
        let started = Instant::now();
        let branches = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                Ok(branches_to_index(&store.repo(id)?, &extra)?)
            })
            .await??
        };
        let wanted = generation.map_or(0, |g| u64::try_from(g).unwrap_or(0));
        // Reading shard metadata can block, so ask off the async threads.
        let held = {
            let index = index.clone();
            let rule = always_index.clone();
            tokio::task::spawn_blocking(move || {
                (
                    index.branches(id),
                    index.ranks_generation(id),
                    index.follows_always_index(id, &rule),
                )
            })
            .await?
        };
        if !force
            && !branches.is_empty()
            && held.0.as_ref() == Some(&branches)
            && held.1 == Some(wanted)
            && held.2
        {
            return Ok(IndexUpdate::Unchanged);
        }
        // The ranks are read only when something is built.
        let ranks = match generation {
            Some(_) => RankingStore::new(&inner.db)
                .path_ranks(id)
                .await?
                .map(Arc::new),
            None => None,
        };
        let req = BuildRequest {
            repo: id,
            name: repo.name.clone(),
            git_dir: store.layout().repo_dir(id),
            branches,
            ranks,
            always_index,
        };
        let update = tokio::task::spawn_blocking(move || {
            if force && !req.branches.is_empty() {
                Ok(IndexUpdate::Built(index.build(&req)?))
            } else {
                update_index(&*index, &req)
            }
        })
        .await??;
        if let IndexUpdate::Built(r) = &update {
            let secs = started.elapsed().as_secs_f64();
            metrics::histogram!(metric_names::INDEX_BUILD_DURATION_SECONDS).record(secs);
            tracing::info!(
                repo = %id,
                name = %repo.name,
                docs = r.docs,
                content_bytes = r.content_bytes,
                index_bytes = r.index_bytes,
                secs,
                "indexed repo"
            );
        }
        Ok(update)
    }

    /// Queues every repo on disk and drops shards of repos that are not. Returns the number
    /// of repos queued.
    pub(crate) fn reconcile_index(&self) -> anyhow::Result<usize> {
        let inner = &*self.inner;
        let Some(index) = &inner.index else {
            return Ok(0);
        };
        let on_disk: Vec<RepoId> = inner
            .store
            .layout()
            .list_repos()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in index.indexed_repos() {
            if !on_disk.contains(&id) {
                index.remove(id)?;
            }
        }
        for id in &on_disk {
            inner.index_queue.push(*id, Duration::ZERO);
        }
        Ok(on_disk.len())
    }

    /// Packs small repos into compound shards.
    pub async fn merge_index(&self) -> anyhow::Result<MergeReport> {
        let Some(index) = self.inner.shards.clone() else {
            // Without an index there is nothing to pack.
            return Ok(MergeReport::default());
        };
        let started = Instant::now();
        let report = tokio::task::spawn_blocking(move || index.merge_compounds()).await??;
        if report != MergeReport::default() {
            tracing::info!(
                shards = report.shards,
                repos = report.repos,
                bytes = report.bytes,
                removed = report.removed,
                secs = started.elapsed().as_secs_f64(),
                "packed compound shards"
            );
        }
        Ok(report)
    }

    /// Writes the outcome of one index update to `search_index_repos` for the site admin. Failures
    /// to write are logged; they do not stop indexing.
    async fn report_index(
        &self,
        id: RepoId,
        result: &anyhow::Result<IndexUpdate>,
        reported: &mut HashSet<RepoId>,
    ) {
        let inner = &*self.inner;
        let store = inner.db.search_index();
        let me = inner.me.as_str();
        let written = match result {
            Ok(IndexUpdate::Unchanged) if reported.contains(&id) => return,
            Ok(IndexUpdate::Built(_) | IndexUpdate::Unchanged) => {
                let status = inner.index.as_ref().and_then(|i| i.repo_status(id));
                match status {
                    Some(s) => {
                        let built = matches!(result, Ok(IndexUpdate::Built(_)));
                        let state = IndexedState {
                            branches: s
                                .branches
                                .into_iter()
                                .map(|b| IndexedBranchRow {
                                    name: b.name,
                                    commit: b.commit,
                                })
                                .collect(),
                            docs: saturating(s.docs),
                            content_bytes: saturating(s.content_bytes),
                            index_bytes: saturating(s.index_bytes),
                            lines: saturating(s.lines),
                            default_branch_lines: saturating(s.default_branch_lines),
                            shards: i32::try_from(s.shards.len()).unwrap_or(i32::MAX),
                            // An unchanged index keeps its time; a first report of one uses
                            // the shard files' time.
                            indexed_at: if built {
                                Some(chrono::Utc::now())
                            } else {
                                s.updated_at.map(chrono::DateTime::<chrono::Utc>::from)
                            },
                        };
                        store.record_indexed(id, me, &state).await
                    }
                    None => store.record_not_indexed(id, me).await,
                }
            }
            Ok(IndexUpdate::Removed) => store.record_not_indexed(id, me).await,
            Err(e) => store.record_error(id, me, &format!("{e:#}")).await,
        };
        match written {
            // Only the repo's owner reports: a copy being moved here, or the old owner's copy
            // kept until its removal (or indexed by a build that ended after the flip), must
            // not overwrite the owner's row; the store skips those writes.
            Ok(false) => {
                tracing::debug!(repo = %id, "not reporting the index of a repo assigned elsewhere");
                reported.remove(&id);
            }
            Ok(true) => {
                if result.is_ok() {
                    reported.insert(id);
                } else {
                    reported.remove(&id);
                }
            }
            Err(e) => tracing::warn!(repo = %id, error = %e, "recording the index state failed"),
        }
    }

    /// Runs the indexing queue until `cancel` fires, with a reconcile pass at start and every
    /// reconcile interval.
    pub(crate) async fn run_indexer(self, cancel: CancellationToken) {
        let inner = &*self.inner;
        if inner.index.is_none() {
            return;
        }
        let mut next_reconcile = Instant::now();
        let mut merge_due = false;
        // Repos whose index state was written to the database since start; an unchanged index
        // is written once, so the rows of an existing index fill in after an upgrade without
        // a write per repo on every reconcile pass.
        let mut reported: HashSet<RepoId> = HashSet::new();
        loop {
            if Instant::now() >= next_reconcile {
                if let Err(e) = self.reconcile_index() {
                    tracing::warn!(error = %e, "index reconcile failed");
                }
                next_reconcile = Instant::now() + inner.cfg.index_reconcile_interval;
                merge_due = true;
            }
            let (ready, next) = inner.index_queue.take_due();
            metrics::gauge!(metric_names::INDEX_QUEUE_LENGTH).set(inner.index_queue.len() as f64);
            for (id, force) in ready {
                inner.index_queue.start(id);
                let run = self.index_repo(id, force);
                let result = tokio::select! {
                    () = cancel.cancelled() => return,
                    r = run => r,
                };
                // A failed build counts as a build for the debounce, so a repo whose build
                // fails is not retried on every push.
                inner
                    .index_queue
                    .finish(id, matches!(result, Ok(IndexUpdate::Built(_)) | Err(_)));
                let outcome = match &result {
                    Ok(IndexUpdate::Built(_)) => "built",
                    Ok(IndexUpdate::Unchanged) => "unchanged",
                    Ok(IndexUpdate::Removed) => "removed",
                    Err(_) => "error",
                };
                metrics::counter!(metric_names::INDEX_UPDATES_TOTAL, "outcome" => outcome)
                    .increment(1);
                if let Err(e) = &result {
                    tracing::warn!(repo = %id, error = %e, "indexing failed");
                }
                self.report_index(id, &result, &mut reported).await;
            }
            if std::mem::take(&mut merge_due) {
                let merge = self.merge_index();
                tokio::select! {
                    () = cancel.cancelled() => return,
                    r = merge => if let Err(e) = r {
                        tracing::warn!(error = %e, "packing compound shards failed");
                    },
                }
            }
            let wait = next
                .map_or(next_reconcile, |n| n.min(next_reconcile))
                .saturating_duration_since(Instant::now());
            tokio::select! {
                () = cancel.cancelled() => return,
                () = inner.index_queue.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }
}

fn saturating(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The site admin's view of the indexer (`IndexStatus`, `Reindex` RPCs).
impl IndexControl for RepoNode {
    fn queued(&self, repo: RepoId) -> bool {
        self.inner.index_queue.contains(repo)
    }

    fn reindex(&self, repo: RepoId) {
        if self.inner.index.is_some() {
            self.inner.index_queue.push_forced(repo);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_keeps_the_earliest_due_time() {
        let q = IndexQueue::default();
        q.push(RepoId(1), Duration::from_secs(60));
        q.push(RepoId(2), Duration::ZERO);
        q.push(RepoId(1), Duration::from_secs(120));
        let (ready, next) = q.take_due();
        assert_eq!(ready, [(RepoId(2), false)]);
        let next = next.unwrap();
        assert!(next <= Instant::now() + Duration::from_secs(60));
        // A shorter delay moves the due time up.
        q.push(RepoId(1), Duration::ZERO);
        assert_eq!(q.take_due(), (vec![(RepoId(1), false)], None));
        assert_eq!(q.len(), 0);
    }

    /// Takes the due repos and runs each as an update that built (or not), as the indexer
    /// does.
    fn run_due(q: &IndexQueue, built: bool) -> Vec<RepoId> {
        let (ready, _) = q.take_due();
        for (id, _) in &ready {
            q.start(*id);
            q.finish(*id, built);
        }
        ready.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn debounce_first_push_builds_at_once() {
        let q = IndexQueue::new(Duration::from_secs(60));
        q.push_debounced(RepoId(1));
        assert_eq!(run_due(&q, true), [RepoId(1)]);
        // Another repo is not held up by repo 1's window.
        q.push_debounced(RepoId(2));
        assert_eq!(run_due(&q, true), [RepoId(2)]);
    }

    #[test]
    fn debounce_coalesces_a_burst_into_one_trailing_build() {
        let window = Duration::from_millis(200);
        let q = IndexQueue::new(window);
        q.push_debounced(RepoId(1));
        let built_at = Instant::now();
        assert_eq!(run_due(&q, true), [RepoId(1)]);
        for _ in 0..5 {
            q.push_debounced(RepoId(1));
        }
        let (ready, next) = q.take_due();
        assert!(ready.is_empty());
        assert_eq!(q.len(), 1);
        let next = next.unwrap();
        assert!(next >= built_at + window - Duration::from_millis(50));
        assert!(next <= Instant::now() + window);
        std::thread::sleep(window + Duration::from_millis(20));
        assert_eq!(run_due(&q, true), [RepoId(1)]);
        assert_eq!(q.len(), 0);
        // Once a window passes with no build, the next push is on the leading edge again.
        std::thread::sleep(window + Duration::from_millis(20));
        q.push_debounced(RepoId(1));
        assert_eq!(run_due(&q, true), [RepoId(1)]);
    }

    #[test]
    fn debounce_never_loses_a_push_during_a_build() {
        let window = Duration::from_millis(100);
        let q = IndexQueue::new(window);
        q.push_debounced(RepoId(1));
        let (ready, _) = q.take_due();
        assert_eq!(ready, [(RepoId(1), false)]);
        q.start(RepoId(1));
        // A push lands while the build runs (it may have read the refs already).
        q.push_debounced(RepoId(1));
        q.finish(RepoId(1), true);
        assert!(q.contains(RepoId(1)));
        assert!(q.take_due().0.is_empty());
        std::thread::sleep(window + Duration::from_millis(20));
        assert_eq!(run_due(&q, true), [RepoId(1)]);
    }

    #[test]
    fn debounce_ignores_updates_that_built_nothing() {
        let q = IndexQueue::new(Duration::from_secs(60));
        // A reconcile check that found the index current does not open a window.
        q.push(RepoId(1), Duration::ZERO);
        assert_eq!(run_due(&q, false), [RepoId(1)]);
        q.push_debounced(RepoId(1));
        assert_eq!(run_due(&q, true), [RepoId(1)]);
        // A push during such a check waits for it, then is due at once.
        q.push(RepoId(2), Duration::ZERO);
        let (ready, _) = q.take_due();
        assert_eq!(ready, [(RepoId(2), false)]);
        q.start(RepoId(2));
        q.push_debounced(RepoId(2));
        assert!(q.take_due().0.is_empty());
        q.finish(RepoId(2), false);
        assert_eq!(run_due(&q, true), [RepoId(2)]);
        // Zero-delay pushes (clones, a site admin's reindex) skip the window.
        q.push_forced(RepoId(2));
        assert_eq!(q.take_due().0, [(RepoId(2), true)]);
    }

    #[test]
    fn forced_rebuilds_are_due_now_and_flagged_once() {
        let q = IndexQueue::default();
        q.push(RepoId(3), Duration::from_secs(60));
        assert!(q.contains(RepoId(3)));
        q.push_forced(RepoId(3));
        assert_eq!(q.take_due(), (vec![(RepoId(3), true)], None));
        assert!(!q.contains(RepoId(3)));
        q.push(RepoId(3), Duration::ZERO);
        assert_eq!(q.take_due(), (vec![(RepoId(3), false)], None));
    }
}
