//! Repo node rebalancing: when repo nodes are added or removed, the repos whose rendezvous-hash
//! owner changed are copied to the new owner, verified, flipped, and then removed from the old
//! owner. The old owner keeps serving until the flip, and for a grace period after it while routing
//! tables reload, so a move has no downtime and puts no load on the code host.
//!
//! Every repo node runs [`RepoNode::run_rebalancer`]. It sends a heartbeat with the node list it
//! runs with (`repo_nodes`) every rebalance interval from a task of its own, so slow cleanup or
//! planning never makes the node look down, and each pass:
//! - plans, on the first live node only and under an advisory lock: when every configured node is
//!   alive with the same list ([`unsettled`] is empty), it queues a move for each misplaced repo
//!   ([`plan`]), by copy when the old owner is alive and by re-clone when it is gone, and cancels
//!   moves that no longer match the hash. A repo pinned in site config
//!   (`repoNodes.pinnedRepos`, repo name → node) is misplaced anywhere but on
//!   its pin, and once unpinned anywhere but on its hash owner;
//! - as the new owner, claims queued moves to itself and runs them, a few at a time: rounds of
//!   `CopyRepo` from the old owner (a ref snapshot plus a pack of what is missing, SHA-256
//!   checked, connectivity and refs verified by [`GitStore::copy_in`](sourcegull_gitstore::GitStore::copy_in))
//!   until a round brings nothing new, an index build, then the flip: `repo.shard` and the
//!   node rows change in one transaction with `NOTIFY repo_shards`, and a fetch is queued to
//!   catch up with anything the old owner fetched after the last round;
//! - as the old owner, removes its copies of repos flipped away at least the grace period
//!   ago, and (every planning interval) copies nobody needs any more.
//!
//! Failed attempts are retried with backoff and end as `failed` after the last one; site
//! admins see the moves on the repo nodes page and can retry failed ones.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use chrono::Utc;
use sha2::Digest as _;
use sourcegull_db::{RepoMove, RepoUpdatePriority, RepoUpdateReason};
use sourcegull_gitstore::{CopyHeader, ObjectId, SyncOutcome};
use sourcegull_rpc::Router;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_types::metric_names;
use sourcegull_types::{CloneStatus, Repo, RepoId};
use tokio::io::AsyncWriteExt as _;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::RepoNode;
use super::rebalance_plan::{live_nodes, plan, unsettled};

/// Moves the planner queues per pass at most; the rest follow in later passes.
const MAX_NEW_PER_PASS: usize = 10_000;
/// Copy rounds before flipping anyway (the catch-up fetch after the flip closes the gap).
const MAX_ROUNDS: usize = 4;
/// How often a running copy's byte count is written (and its cancellation checked).
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Finished moves and nodes gone for this long are forgotten.
const RETENTION: chrono::Duration = chrono::Duration::days(7);
/// Old copies removed per pass.
const GC_BATCH: i64 = 500;

/// Moves by outcome (`flipped`, `reassigned`, `cancelled`, `retry`, `failed`).
pub const MOVES_METRIC: &str = metric_names::REPO_MOVES_TOTAL;
/// Pack bytes received by new owners.
pub const MOVE_BYTES_METRIC: &str = metric_names::REPO_MOVE_BYTES_TOTAL;
/// Time from claiming a move to its flip.
pub const MOVE_DURATION_METRIC: &str = metric_names::REPO_MOVE_DURATION_SECONDS;
/// Old copies removed after a move (or found unneeded).
pub const OLD_COPIES_METRIC: &str = metric_names::REPO_MOVE_OLD_COPIES_REMOVED_TOTAL;

/// What a planning pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanReport {
    /// Configured nodes that are not alive with the same node list; nothing is planned while
    /// any are.
    pub waiting_for: Vec<String>,
    /// Whether another node planned (this node is not the first live node, or holds no lock).
    pub skipped: bool,
    /// Moves queued.
    pub queued: u64,
    /// Moves cancelled.
    pub cancelled: u64,
    /// Misplaced repos left for the next pass.
    pub deferred: usize,
}

/// How a move ended on the new owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    /// Copied, verified and flipped.
    Flipped,
    /// Reassigned without a copy (the old owner is gone or never cloned it); a clone is queued.
    Reassigned,
    /// No longer wanted.
    Cancelled,
}

/// The move was cancelled while copying.
#[derive(Debug, thiserror::Error)]
#[error("the move was cancelled")]
struct Cancelled;

/// Backoff before attempt `attempts + 1`: 30 s doubling, at most an hour.
fn backoff(attempts: i32) -> Duration {
    let exp = u32::try_from(attempts.saturating_sub(1).clamp(0, 7)).unwrap_or(0);
    Duration::from_secs(30 * 2u64.pow(exp)).min(Duration::from_secs(3600))
}

/// The configured node `pin` names: the exact address, or one that equals it once its scheme is
/// stripped (`repo-2.repo:3178` for `http://repo-2.repo:3178`).
fn pin_node<'a>(router: &'a Router, pin: &str) -> Option<&'a str> {
    let bare = |n: &str| n.split_once("://").map_or(n, |(_, rest)| rest).to_owned();
    router
        .nodes()
        .find(|n| *n == pin)
        .or_else(|| router.nodes().find(|n| bare(n) == bare(pin)))
}

/// `router` with the site config pins (repo name → node) of the repos in `ids` (lowercased
/// name → id) applied. Pins to nodes that are not configured are ignored with a warning.
fn pinned_router(
    router: &Router,
    pins: &std::collections::BTreeMap<String, String>,
    ids: &[(String, RepoId)],
) -> Router {
    let mut out = router.clone();
    if pins.is_empty() {
        return out;
    }
    let ids: std::collections::HashMap<&str, RepoId> =
        ids.iter().map(|(n, id)| (n.as_str(), *id)).collect();
    for (name, pin) in pins {
        let Some(id) = ids.get(name.to_lowercase().as_str()) else {
            tracing::debug!(repo = %name, "pinned repository not found");
            continue;
        };
        match pin_node(router, pin) {
            Some(node) => out.pin(*id, node.to_owned()),
            None => tracing::warn!(
                repo = %name, node = %pin,
                "ignoring the pin of a repository to a repo node that is not configured"
            ),
        }
    }
    out
}

impl RepoNode {
    /// Heartbeats are older than this when a node counts as down.
    fn live_window(&self) -> Duration {
        (self.inner.cfg.rebalance_interval * 6).max(Duration::from_secs(5))
    }

    /// Records this node's heartbeat.
    pub async fn rebalance_heartbeat(&self) -> anyhow::Result<()> {
        let inner = &*self.inner;
        inner
            .db
            .repo_moves()
            .heartbeat(&inner.me, &inner.cfg.nodes, inner.started_at)
            .await?;
        Ok(())
    }

    /// One planning pass (see the module docs). Only the first live node plans.
    pub async fn plan_rebalance(&self) -> anyhow::Result<PlanReport> {
        let inner = &*self.inner;
        let store = inner.db.repo_moves();
        let heartbeats = store.heartbeats().await?;
        let now = store.now().await?;
        let live = live_nodes(&heartbeats, now, self.live_window());
        let mut report = PlanReport {
            waiting_for: unsettled(&inner.cfg.nodes, &heartbeats, &live),
            ..PlanReport::default()
        };
        if !report.waiting_for.is_empty() {
            return Ok(report);
        }
        let first = inner.cfg.nodes.iter().filter(|n| live.contains(*n)).min();
        if first != Some(&inner.me) {
            report.skipped = true;
            return Ok(report);
        }
        let Some(mut planning) = store.begin_planning().await? else {
            report.skipped = true;
            return Ok(report);
        };
        let assignments = planning.assignments().await?;
        let open = planning.open_moves().await?;
        let pins = inner.site.load().value.pinned_repos();
        let names: Vec<String> = pins.keys().cloned().collect();
        let ids = planning.repo_ids_by_name(&names).await?;
        let router = pinned_router(&inner.router, &pins, &ids);
        let p = plan(&router, &live, &assignments, &open, MAX_NEW_PER_PASS);
        report.cancelled = planning
            .cancel(&p.cancel, "the repository's owner changed again")
            .await?;
        report.queued = planning.queue(&p.queue).await?;
        report.deferred = p.deferred;
        let live: Vec<String> = live.into_iter().collect();
        planning.finish_orphaned(&live).await?;
        planning.prune(now - RETENTION).await?;
        planning.commit().await?;
        store.forget_nodes(now - RETENTION).await?;
        if report.queued > 0 || report.cancelled > 0 {
            tracing::info!(
                queued = report.queued,
                cancelled = report.cancelled,
                deferred = report.deferred,
                "planned repo moves"
            );
        }
        Ok(report)
    }

    /// Claims up to `limit` moves to this node.
    pub async fn claim_moves(&self, limit: usize) -> anyhow::Result<Vec<RepoMove>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        Ok(self
            .inner
            .db
            .repo_moves()
            .claim(&self.inner.me, limit)
            .await?)
    }

    /// Runs one claimed move and records its outcome; errors become a retry or `failed`.
    pub async fn run_move(&self, m: RepoMove, cancel: CancellationToken) -> Option<MoveOutcome> {
        let inner = &*self.inner;
        let started = Instant::now();
        let result = tokio::select! {
            r = self.move_repo(&m, &cancel) => r,
            () = cancel.cancelled() => return None,
        };
        let store = inner.db.repo_moves();
        match result {
            Ok(outcome) => {
                let label = match outcome {
                    MoveOutcome::Flipped => {
                        metrics::histogram!(MOVE_DURATION_METRIC)
                            .record(started.elapsed().as_secs_f64());
                        "flipped"
                    }
                    MoveOutcome::Reassigned => "reassigned",
                    MoveOutcome::Cancelled => "cancelled",
                };
                metrics::counter!(MOVES_METRIC, "outcome" => label).increment(1);
                tracing::info!(
                    repo = %m.repo_id, name = %m.repo_name, from = %m.from_node, outcome = label,
                    secs = started.elapsed().as_secs_f64(), "repo move finished"
                );
                Some(outcome)
            }
            Err(e) if e.is::<Cancelled>() => {
                let _ = store
                    .cancel(m.repo_id, &inner.me, "cancelled while copying")
                    .await;
                self.drop_copy_unless_owned(m.repo_id).await;
                metrics::counter!(MOVES_METRIC, "outcome" => "cancelled").increment(1);
                Some(MoveOutcome::Cancelled)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let retry_at = (u32::try_from(m.attempts).unwrap_or(u32::MAX)
                    < inner.cfg.move_max_attempts)
                    .then(|| Utc::now() + backoff(m.attempts));
                let label = if retry_at.is_some() {
                    "retry"
                } else {
                    "failed"
                };
                tracing::warn!(
                    repo = %m.repo_id, name = %m.repo_name, from = %m.from_node,
                    attempt = m.attempts, error = %msg, "repo move failed"
                );
                metrics::counter!(MOVES_METRIC, "outcome" => label).increment(1);
                if let Err(e) = store.fail(m.repo_id, &inner.me, &msg, retry_at).await {
                    tracing::warn!(repo = %m.repo_id, error = %e, "recording a failed move failed");
                }
                None
            }
        }
    }

    /// Moves one repo to this node (see the module docs).
    async fn move_repo(
        &self,
        m: &RepoMove,
        cancel: &CancellationToken,
    ) -> anyhow::Result<MoveOutcome> {
        let inner = &*self.inner;
        let db = &inner.db;
        let moves = db.repo_moves();
        let id = m.repo_id;
        let Some(repo) = db.repos().get(id).await? else {
            moves
                .cancel(id, &inner.me, "the repository was deleted")
                .await?;
            self.drop_copy_unless_owned(id).await;
            return Ok(MoveOutcome::Cancelled);
        };
        if repo.shard.as_deref() != Some(m.from_node.as_str()) {
            moves
                .cancel(id, &inner.me, "the repository is no longer on the old node")
                .await?;
            self.drop_copy_unless_owned(id).await;
            return Ok(MoveOutcome::Cancelled);
        }
        let cloned = db
            .gitserver_repos()
            .get(id)
            .await?
            .is_some_and(|g| g.clone_status == CloneStatus::Cloned);
        if m.kind == "reclone" || !cloned || !self.node_alive(&m.from_node).await? {
            return self.reassign(m).await;
        }
        match self.copy_and_flip(m, &repo, cancel).await {
            // The old node went away mid-move (a scale-in that did not wait): clone instead.
            Err(e) if !e.is::<Cancelled>() && !self.node_alive(&m.from_node).await? => {
                tracing::info!(
                    repo = %id, from = %m.from_node, error = format!("{e:#}"),
                    "the old node is gone; cloning the repo from its code host instead"
                );
                self.reassign(m).await
            }
            r => r,
        }
    }

    /// Takes a repo over without a copy and queues its clone.
    async fn reassign(&self, m: &RepoMove) -> anyhow::Result<MoveOutcome> {
        let inner = &*self.inner;
        if !inner
            .db
            .repo_moves()
            .reassign_without_copy(m.repo_id, &m.from_node, &inner.me)
            .await?
        {
            return Ok(MoveOutcome::Cancelled);
        }
        inner
            .db
            .repo_update_jobs()
            .enqueue(
                m.repo_id,
                RepoUpdatePriority::NORMAL,
                RepoUpdateReason::Scheduled,
            )
            .await?;
        Ok(MoveOutcome::Reassigned)
    }

    /// Whether `node` sent a heartbeat within the live window.
    async fn node_alive(&self, node: &str) -> anyhow::Result<bool> {
        let store = self.inner.db.repo_moves();
        let heartbeats = store.heartbeats().await?;
        let now = store.now().await?;
        Ok(live_nodes(&heartbeats, now, self.live_window()).contains(node))
    }

    /// Copies the repo from its old owner in rounds until one brings nothing new, indexes it,
    /// and flips the assignment.
    async fn copy_and_flip(
        &self,
        m: &RepoMove,
        repo: &Repo,
        cancel: &CancellationToken,
    ) -> anyhow::Result<MoveOutcome> {
        let inner = &*self.inner;
        let db = &inner.db;
        let id = m.repo_id;
        let mut client = inner
            .peers
            .for_node(&m.from_node)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut last: Option<SyncOutcome> = None;
        // Pack bytes received over all rounds.
        let mut copied: u64 = 0;
        for round in 0..MAX_ROUNDS {
            if cancel.is_cancelled() {
                bail!("shutting down");
            }
            let outcome = self
                .copy_round(&mut client, repo, &mut copied)
                .await
                .with_context(|| format!("copy round {} from {}", round + 1, m.from_node))?;
            let changed = outcome.cloned || !outcome.ref_updates.is_empty();
            if (round == 0 || changed) && inner.index.is_some() {
                self.index_repo(id, false)
                    .await
                    .context("indexing the copy")?;
            }
            last = Some(outcome);
            if round > 0 && !changed {
                break;
            }
        }
        let size = last.map(|o| i64::try_from(o.size_bytes).unwrap_or(i64::MAX));
        if !db
            .repo_moves()
            .flip(id, &m.from_node, &inner.me, size)
            .await?
        {
            self.drop_copy_unless_owned(id).await;
            return Ok(MoveOutcome::Cancelled);
        }
        // Catch up with fetches the old owner did after the last round, and report the index.
        db.repo_update_jobs()
            .enqueue(id, RepoUpdatePriority::NORMAL, RepoUpdateReason::Scheduled)
            .await?;
        self.queue_index(id, Duration::ZERO);
        Ok(MoveOutcome::Flipped)
    }

    /// One `CopyRepo` round from the old owner into this node's store.
    async fn copy_round(
        &self,
        client: &mut sourcegull_rpc::repo::v1::repo_service_client::RepoServiceClient<
            sourcegull_rpc::tonic::transport::Channel,
        >,
        repo: &Repo,
        copied: &mut u64,
    ) -> anyhow::Result<SyncOutcome> {
        use pb::copy_repo_response::Part;
        let inner = &*self.inner;
        let store = &inner.store;
        let have: Vec<String> = store
            .ref_snapshot(repo.id)
            .await?
            .map(|s| {
                let set: HashSet<ObjectId> = s.into_values().collect();
                set.into_iter().map(|o| o.to_string()).collect()
            })
            .unwrap_or_default();
        let mut stream = client
            .copy_repo(pb::CopyRepoRequest {
                repo: Some(pb::RepoRef {
                    id: repo.id.get(),
                    name: repo.name.clone(),
                }),
                have,
            })
            .await
            .map_err(|s| anyhow::anyhow!("{}", s.message()))?
            .into_inner();
        let header = match stream.message().await? {
            Some(pb::CopyRepoResponse {
                part: Some(Part::Header(h)),
            }) => h,
            _ => bail!("the old node sent no header"),
        };
        let mut pack = None;
        if header.has_pack {
            let path = tempfile::Builder::new()
                .prefix(&format!("move-{}-", repo.id.get()))
                .suffix(".pack")
                .tempfile_in(store.layout().tmp_dir())?
                .into_temp_path();
            let file = tokio::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .await?;
            pack = Some((path, file));
        }
        let mut hasher = sha2::Sha256::new();
        let mut bytes: u64 = 0;
        let mut last_progress = Instant::now();
        loop {
            let msg = stream
                .message()
                .await
                .map_err(|s| anyhow::anyhow!("{}", s.message()))?;
            match msg.and_then(|m| m.part) {
                Some(Part::Chunk(chunk)) => {
                    let Some((_, file)) = pack.as_mut() else {
                        bail!("the old node sent pack data it announced none of");
                    };
                    file.write_all(&chunk).await?;
                    hasher.update(&chunk);
                    bytes += chunk.len() as u64;
                    *copied += chunk.len() as u64;
                    metrics::counter!(MOVE_BYTES_METRIC).increment(chunk.len() as u64);
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        let still = inner
                            .db
                            .repo_moves()
                            .progress(
                                repo.id,
                                &inner.me,
                                i64::try_from(*copied).unwrap_or(i64::MAX),
                            )
                            .await?;
                        if !still {
                            return Err(Cancelled.into());
                        }
                    }
                }
                Some(Part::Trailer(t)) => {
                    if t.pack_bytes != bytes || t.sha256 != hasher.finalize().as_slice() {
                        bail!(
                            "the pack arrived damaged ({bytes} bytes received, {} sent)",
                            t.pack_bytes
                        );
                    }
                    break;
                }
                Some(Part::Header(_)) => bail!("the old node sent a second header"),
                None => bail!("the copy stream ended early"),
            }
        }
        if let Some((_, file)) = pack.as_mut() {
            file.flush().await?;
            file.sync_all().await?;
        }
        let mut refs = sourcegull_gitstore::refs::RefSnapshot::new();
        for r in header.refs {
            let oid = ObjectId::from_hex(r.oid.as_bytes())
                .with_context(|| format!("bad object id for {}", r.name))?;
            refs.insert(r.name, oid);
        }
        let header = CopyHeader {
            refs,
            head_ref: Some(header.head_ref).filter(|h| !h.is_empty()),
            origin_url: Some(header.origin_url).filter(|u| !u.is_empty()),
        };
        inner
            .db
            .repo_moves()
            .progress(
                repo.id,
                &inner.me,
                i64::try_from(*copied).unwrap_or(i64::MAX),
            )
            .await?;
        let pack_path = pack.as_ref().map(|(p, _)| p.to_path_buf());
        let outcome = store
            .copy_in(repo.id, &header, pack_path.as_deref())
            .await?;
        Ok(outcome)
    }

    /// Removes this node's copy of `repo` (and its index shards) unless the repo is assigned
    /// to this node.
    pub(crate) async fn drop_copy_unless_owned(&self, repo: RepoId) {
        let inner = &*self.inner;
        let shard: Option<String> = match sqlx::query_scalar("SELECT shard FROM repo WHERE id = $1")
            .bind(repo)
            .fetch_optional(inner.db.pool())
            .await
        {
            Ok(s) => s.flatten(),
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "checking a repo's owner failed");
                return;
            }
        };
        if shard.as_deref() == Some(inner.me.as_str()) {
            return;
        }
        match inner.store.delete(repo).await {
            Ok(true) => {
                metrics::counter!(OLD_COPIES_METRIC).increment(1);
                self.queue_index(repo, Duration::ZERO);
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(repo = %repo, error = %e, "removing a repo copy failed"),
        }
    }

    /// As the old owner: removes copies of repos flipped away at least the grace period ago
    /// and marks their moves done. Returns the repos whose moves finished.
    pub async fn remove_moved_away(&self) -> anyhow::Result<Vec<RepoId>> {
        let inner = &*self.inner;
        let moves = inner.db.repo_moves();
        let ids = moves
            .flipped_from(&inner.me, inner.cfg.move_gc_grace, GC_BATCH)
            .await?;
        for id in &ids {
            self.drop_copy_unless_owned(*id).await;
            moves.finish(*id).await?;
        }
        if !ids.is_empty() {
            tracing::info!(repos = ids.len(), "removed repos moved to other nodes");
        }
        Ok(ids)
    }

    /// Removes copies on this node's disk that nobody needs (see
    /// [`RepoMoveStore::stray_copies`](sourcegull_db::RepoMoveStore::stray_copies)).
    pub async fn remove_stray_copies(&self) -> anyhow::Result<Vec<RepoId>> {
        let inner = &*self.inner;
        let on_disk: Vec<RepoId> = inner
            .store
            .layout()
            .list_repos()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let strays = inner
            .db
            .repo_moves()
            .stray_copies(&inner.me, &on_disk)
            .await?;
        for id in &strays {
            tracing::info!(repo = %id, "removing a copy of a repo stored on another node");
            self.drop_copy_unless_owned(*id).await;
        }
        Ok(strays)
    }

    /// Runs the rebalancer until `cancel` fires (see the module docs).
    pub(crate) async fn run_rebalancer(self, cancel: CancellationToken) {
        let inner = &*self.inner;
        let concurrency = inner.cfg.rebalance_concurrency;
        match inner.db.repo_moves().release(&inner.me).await {
            Ok(n) if n > 0 => tracing::info!(moves = n, "requeued interrupted repo moves"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "requeueing interrupted repo moves failed"),
        }
        if let Err(e) = self.rebalance_heartbeat().await {
            tracing::warn!(error = %e, "repo node heartbeat failed");
        }
        let heartbeats = tokio::spawn(self.clone().run_heartbeats(cancel.clone()));
        let mut tick = tokio::time::interval(inner.cfg.rebalance_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_plan = Instant::now();
        let mut running: JoinSet<()> = JoinSet::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {}
                Some(_) = running.join_next(), if !running.is_empty() => {}
            }
            if Instant::now() >= next_plan {
                next_plan = Instant::now() + inner.cfg.rebalance_plan_interval;
                if concurrency > 0 {
                    match self.plan_rebalance().await {
                        Ok(r) if !r.waiting_for.is_empty() => tracing::debug!(
                            waiting_for = ?r.waiting_for,
                            "not planning repo moves until every repo node runs with the same node list"
                        ),
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = format!("{e:#}"), "planning repo moves failed")
                        }
                    }
                }
                if let Err(e) = self.remove_stray_copies().await {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "removing unneeded repo copies failed"
                    );
                }
            }
            if let Err(e) = self.remove_moved_away().await {
                tracing::warn!(error = format!("{e:#}"), "removing moved repos failed");
            }
            let free = concurrency.saturating_sub(running.len());
            if free == 0 {
                continue;
            }
            match self.claim_moves(free).await {
                Ok(moves) => {
                    for m in moves {
                        let node = self.clone();
                        let cancel = cancel.clone();
                        running.spawn(async move {
                            node.run_move(m, cancel).await;
                        });
                    }
                }
                Err(e) => tracing::warn!(error = format!("{e:#}"), "claiming repo moves failed"),
            }
        }
        running.abort_all();
        heartbeats.abort();
    }

    /// Sends the heartbeat every rebalance interval until `cancel` fires. The first one is
    /// due one interval from now.
    async fn run_heartbeats(self, cancel: CancellationToken) {
        let period = self.inner.cfg.rebalance_interval;
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            if let Err(e) = self.rebalance_heartbeat().await {
                tracing::warn!(error = %e, "repo node heartbeat failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_an_hour() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(4), Duration::from_secs(240));
        assert_eq!(backoff(50), Duration::from_secs(3600));
        assert_eq!(backoff(0), Duration::from_secs(30));
    }

    #[test]
    fn site_config_pins_resolve_to_configured_nodes() {
        let router = Router::new(["http://repo-0.repo:3178", "http://repo-1.repo:3178"]);
        let pins = [
            ("GitLab.com/Acme/Big", "repo-1.repo:3178"),
            ("gitlab.com/acme/exact", "http://repo-0.repo:3178"),
            ("gitlab.com/acme/elsewhere", "http://repo-9.repo:3178"),
            ("gitlab.com/acme/missing", "http://repo-0.repo:3178"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect();
        let ids = [
            ("gitlab.com/acme/big".to_owned(), RepoId(1)),
            ("gitlab.com/acme/exact".to_owned(), RepoId(2)),
            ("gitlab.com/acme/elsewhere".to_owned(), RepoId(3)),
        ];
        let r = pinned_router(&router, &pins, &ids);
        assert_eq!(r.node_for(RepoId(1)), Some("http://repo-1.repo:3178"));
        assert_eq!(r.node_for(RepoId(2)), Some("http://repo-0.repo:3178"));
        assert_eq!(r.node_for(RepoId(3)), router.hashed_node_for(RepoId(3)));
    }
}
