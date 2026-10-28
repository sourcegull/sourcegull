//! The `repo_update_jobs` consumer.
//!
//! Each node dequeues only jobs for repos assigned to it (`repo.shard`) or not assigned yet, with
//! the queue's `FOR UPDATE SKIP LOCKED` dequeue plus an ownership filter. An unassigned repo is
//! assigned to its hash owner first; a job for a repo owned by another node is handed over by
//! re-enqueueing it. The job then clones (not on disk), re-clones (marked corrupt) or fetches the
//! repo and records the outcome: `gitserver_repos` status, progress, size and head,
//! `repo_ref_events` for refs a fetch actually moved, and the fetch schedule. Clones and fetches
//! that moved refs queue the repo for reindexing. An update that ends after the repo moved to
//! another node records nothing, and a first clone it made is removed.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_db::{
    FetchResult, FetchSchedule, RefEventSource, RefUpdate, RepoUpdateJob, RepoUpdatePriority,
    RepoUpdateReason,
};
use sourcegull_gitstore::janitor::JanitorTask;
use sourcegull_gitstore::{GitError, RemoteHint, SyncHooks, SyncOutcome};
use sourcegull_pgqueue::{Handler, JobContext, JobError, Record};
use sourcegull_types::metric_names;
use sourcegull_types::{Repo, RepoId};
use tokio_util::sync::CancellationToken;

use super::RepoNode;

/// GitLab's Reporter access level (`metadata.accessLevel`), the lowest that may download a
/// private project's code.
const REPORTER_ACCESS: i64 = 20;
use super::resolver::ResolveError;
use super::scheduler::Policy;

/// How often a running clone's progress line is written to `gitserver_repos`.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);

/// The dequeue filter: jobs for repos assigned to this node (`$3`) or to no node yet.
pub(crate) const OWNERSHIP_FILTER: &str = "EXISTS (SELECT 1 FROM repo r \
     WHERE r.id = repo_update_jobs.repo_id AND (r.shard = $3::text OR r.shard IS NULL))";

/// Runs repo update jobs on a [`RepoNode`].
pub(crate) struct UpdateHandler {
    pub(crate) node: RepoNode,
}

#[async_trait]
impl Handler<RepoUpdateJob> for UpdateHandler {
    async fn handle(&self, ctx: JobContext, record: Record<RepoUpdateJob>) -> Result<(), JobError> {
        let job = record.payload;
        let node = &self.node;
        let db = &node.inner.db;
        let Some(repo) = db.repos().get(job.repo_id).await.map_err(JobError::retry)? else {
            // Deleted: the janitor removes it from disk after the grace period.
            return Ok(());
        };
        let owner = match &repo.shard {
            Some(s) => s.clone(),
            None => node.claim(repo.id).await.map_err(JobError::retry)?,
        };
        if owner != node.inner.me {
            tracing::debug!(repo = %repo.id, %owner, "handing the update over to the owning node");
            db.repo_update_jobs()
                .enqueue(
                    repo.id,
                    RepoUpdatePriority(job.priority),
                    parse_reason(&job.reason),
                )
                .await
                .map_err(JobError::retry)?;
            return Ok(());
        }
        if repo.blocked.is_some() {
            return Ok(());
        }
        let force_reclone = parse_reason(&job.reason) == RepoUpdateReason::Reclone;
        node.update_repo(&repo, ctx.cancellation().clone(), force_reclone)
            .await
    }
}

/// The stored reason back as an enum (unknown values count as `scheduled`).
fn parse_reason(s: &str) -> RepoUpdateReason {
    match s {
        "discovered" => RepoUpdateReason::Discovered,
        "activity" => RepoUpdateReason::Activity,
        "webhook" => RepoUpdateReason::Webhook,
        "manual" => RepoUpdateReason::Manual,
        "reclone" => RepoUpdateReason::Reclone,
        _ => RepoUpdateReason::Scheduled,
    }
}

/// Whether a failed fetch's message hints at a damaged object store (worth an fsck).
fn suggests_corruption(e: &GitError) -> bool {
    if matches!(e, GitError::Corrupt(_)) {
        return true;
    }
    let msg = e.to_string().to_ascii_lowercase();
    [
        "corrupt",
        "bad object",
        "missing object",
        "missing blob",
        "missing tree",
        "unable to read",
        "invalid sha1 pointer",
        "loose object",
        "bad packed object",
    ]
    .iter()
    .any(|p| msg.contains(p))
}

fn hex(o: Option<sourcegull_gitstore::ObjectId>) -> Option<String> {
    o.map(|o| o.to_string())
}

impl RepoNode {
    /// Assigns an unassigned repo to its hash owner and returns the repo's owner.
    pub(crate) async fn claim(&self, repo: RepoId) -> Result<String, sourcegull_db::DbError> {
        let inner = &*self.inner;
        if let Some(node) = inner.router.hashed_node_for(repo) {
            inner
                .db
                .repos()
                .assign_shards(&[(repo, node.to_owned())])
                .await?;
        }
        let shard: Option<String> = sqlx::query_scalar("SELECT shard FROM repo WHERE id = $1")
            .bind(repo)
            .fetch_optional(inner.db.pool())
            .await?
            .flatten();
        Ok(shard.unwrap_or_else(|| inner.me.clone()))
    }

    /// Clones, re-clones or fetches `repo` on this node and records the outcome (the work of an
    /// update job once it found the repo assigned here). A cloned repo is re-cloned when it is
    /// marked corrupt or `force_reclone` is set (a site admin's `recloneRepository`).
    pub async fn update_repo(
        &self,
        repo: &Repo,
        cancel: CancellationToken,
        force_reclone: bool,
    ) -> Result<(), JobError> {
        let inner = &*self.inner;
        let id = repo.id;
        let gs = inner.db.gitserver_repos();
        let row = gs.get(id).await.map_err(JobError::retry)?;
        let schedule = gs
            .fetch_schedule(id)
            .await
            .map_err(JobError::retry)?
            .unwrap_or_default();
        let was_cloned = inner.store.is_cloned(id);
        let reclone =
            was_cloned && (force_reclone || row.as_ref().is_some_and(|r| r.corrupted_at.is_some()));

        let remote = match inner.resolver.resolve_id(id).await {
            Ok(r) => r,
            Err(ResolveError::RepoNotFound(_)) => return Ok(()),
            Err(ResolveError::Db(e)) => return Err(JobError::retry(e)),
            Err(e) => {
                let msg = e.to_string();
                self.record_failure(repo, &schedule, &msg).await;
                return Err(JobError::fatal(msg));
            }
        };
        if !was_cloned && inner.clones_paused.load(Ordering::Relaxed) {
            let msg = sourcegull_db::CLONES_PAUSED_ERROR;
            self.record_failure(repo, &schedule, msg).await;
            return Err(JobError::fatal(msg));
        }

        let started = Instant::now();
        if !was_cloned
            && !gs
                .set_cloning(id, &inner.me)
                .await
                .map_err(JobError::retry)?
        {
            tracing::debug!(repo = %id, "not cloning a repo assigned to another node");
            return Ok(());
        }
        let latest: Arc<Mutex<Option<String>>> = Arc::default();
        let progress_stop = CancellationToken::new();
        let progress_task = (!was_cloned || reclone).then(|| {
            tokio::spawn(write_progress(
                self.clone(),
                id,
                latest.clone(),
                progress_stop.clone(),
            ))
        });
        let sink = {
            let latest = latest.clone();
            Arc::new(move |line: &str| {
                let line = line.trim();
                if !line.is_empty() {
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(line.to_owned());
                }
            })
        };
        let result = inner
            .store
            .sync_with(
                id,
                &remote,
                SyncHooks {
                    progress: Some(sink),
                    cancel: Some(cancel.clone()),
                    reclone,
                    remote_hint: self.remote_hint(repo).await,
                },
            )
            .await;
        progress_stop.cancel();
        if let Some(t) = progress_task {
            let _ = t.await;
        }

        let kind = if reclone {
            "reclone"
        } else if was_cloned {
            "fetch"
        } else {
            "clone"
        };
        metrics::histogram!(metric_names::REPO_UPDATE_DURATION_SECONDS, "kind" => kind)
            .record(started.elapsed().as_secs_f64());
        match result {
            Ok(outcome) => {
                metrics::counter!(metric_names::REPO_UPDATES_TOTAL, "kind" => kind, "outcome" => "ok")
                    .increment(1);
                let recorded = self
                    .record_success(repo, &schedule, &outcome, was_cloned)
                    .await
                    .map_err(JobError::retry)?;
                if !recorded {
                    // The repo moved to another node while this update ran: the new owner's
                    // rows stay as they are, and a first clone here is a copy nobody needs.
                    tracing::info!(repo = %id, name = %repo.name, kind, "the repo moved to another node during the update; not recording it");
                    if !was_cloned {
                        self.drop_copy_unless_owned(id).await;
                    }
                    return Ok(());
                }
                if let Some(why) = outcome.remote_unavailable {
                    // No repository the node can fetch (none yet, disabled, or no code access
                    // for the token): nothing to index. The fetch that finds the repository
                    // reports ref updates and queues the index then.
                    tracing::debug!(repo = %id, name = %repo.name, reason = why.as_str(), "no fetchable repository on the code host");
                } else if !was_cloned || reclone {
                    self.queue_index(id, Duration::ZERO);
                } else if !outcome.ref_updates.is_empty() {
                    self.queue_index_debounced(id);
                }
                Ok(())
            }
            Err(e) if cancel.is_cancelled() => {
                if !was_cloned {
                    let _ = gs.reset_cloning(id, &inner.me).await;
                }
                Err(JobError::retry(format!("cancelled: {e}")))
            }
            Err(e) => {
                metrics::counter!(metric_names::REPO_UPDATES_TOTAL, "kind" => kind, "outcome" => "error")
                    .increment(1);
                let msg = e.to_string();
                tracing::warn!(repo = %id, name = %repo.name, kind, error = %msg, "repo update failed");
                self.record_failure(repo, &schedule, &msg).await;
                if was_cloned && !reclone && suggests_corruption(&e) {
                    self.check_corruption(id).await;
                }
                Err(JobError::fatal(msg))
            }
        }
    }

    /// What the code host's last listing said about the project's repository: no repository yet
    /// (GitLab `empty_repo`), the repository feature disabled, or the token below Reporter on a
    /// private project. It classifies a refused fetch.
    async fn remote_hint(&self, repo: &Repo) -> RemoteHint {
        let h = self
            .inner
            .db
            .repos()
            .code_host_hints(repo.id)
            .await
            .unwrap_or_default();
        RemoteHint {
            empty: h.empty,
            repository_disabled: h.repository_disabled,
            below_reporter: repo.private && h.access_level.is_some_and(|l| l < REPORTER_ACCESS),
        }
    }

    /// Records a successful update: `gitserver_repos`, the ref events and the fetch schedule.
    /// Returns `false`, recording nothing, when the repo is no longer assigned to this node.
    async fn record_success(
        &self,
        repo: &Repo,
        prev: &FetchSchedule,
        outcome: &SyncOutcome,
        was_cloned: bool,
    ) -> Result<bool, sourcegull_db::DbError> {
        let inner = &*self.inner;
        let id = repo.id;
        // A first clone reports every ref as created; those are not ref moves. Fetches and
        // re-clones report real changes relative to the previous copy.
        let changed = !outcome.ref_updates.is_empty();
        let gs = inner.db.gitserver_repos();
        let owned = gs
            .record_fetched(
                id,
                &FetchResult {
                    changed,
                    head_oid: hex(outcome.head_oid),
                    size_bytes: i64::try_from(outcome.size_bytes).ok(),
                    node: inner.me.clone(),
                    remote_unavailable: outcome.remote_unavailable,
                },
            )
            .await?;
        if !owned {
            return Ok(false);
        }
        if was_cloned && changed {
            let updates: Vec<RefUpdate> = outcome
                .ref_updates
                .iter()
                .map(|u| RefUpdate {
                    name: u.name.clone(),
                    old_oid: hex(u.old),
                    new_oid: hex(u.new),
                })
                .collect();
            inner
                .db
                .ref_events()
                .record(id, &updates, RefEventSource::Fetch)
                .await?;
        }
        let now = Utc::now();
        let mean = if changed && was_cloned {
            sourcegull_gitstore::schedule::update_mean_change_interval(
                scheduler_mean(prev),
                prev.last_changed,
                now,
            )
            .map(|d| d.as_secs_f64())
            .or(prev.mean_change_interval_secs)
        } else {
            prev.mean_change_interval_secs
        };
        let mut next = FetchSchedule {
            last_fetched: Some(now),
            last_changed: if changed || prev.last_changed.is_none() {
                Some(now)
            } else {
                prev.last_changed
            },
            last_attempt_at: Some(now),
            consecutive_failures: 0,
            mean_change_interval_secs: mean,
            next_fetch_at: None,
        };
        next.next_fetch_at = self.policy().next_due(id, &repo.name, &next, now);
        gs.set_fetch_schedule(id, &next).await?;
        Ok(true)
    }

    async fn record_failure(&self, repo: &Repo, prev: &FetchSchedule, msg: &str) {
        let gs = self.inner.db.gitserver_repos();
        match gs.record_error(repo.id, &self.inner.me, msg).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(repo = %repo.id, "not recording the error of a repo assigned to another node");
                return;
            }
            Err(e) => {
                tracing::warn!(repo = %repo.id, error = %e, "recording the update error failed")
            }
        }
        let now = Utc::now();
        let mut next = FetchSchedule {
            last_attempt_at: Some(now),
            consecutive_failures: prev.consecutive_failures.saturating_add(1),
            ..prev.clone()
        };
        next.next_fetch_at = self.policy().next_due(repo.id, &repo.name, &next, now);
        if let Err(e) = gs.set_fetch_schedule(repo.id, &next).await {
            tracing::warn!(repo = %repo.id, error = %e, "recording the fetch schedule failed");
        }
    }

    /// Runs fsck; a corrupt repo is marked and queued for a re-clone.
    pub(crate) async fn check_corruption(&self, id: RepoId) -> bool {
        let inner = &*self.inner;
        match inner.store.run_janitor(id, JanitorTask::Fsck).await {
            Ok(o) if o.corrupt => {
                self.mark_corrupt(id, "fsck failed after a fetch error")
                    .await;
                true
            }
            Ok(_) => false,
            Err(e) => {
                tracing::warn!(repo = %id, error = %e, "fsck failed to run");
                false
            }
        }
    }

    /// Marks `id` corrupt and queues a re-clone.
    pub(crate) async fn mark_corrupt(&self, id: RepoId, reason: &str) {
        let inner = &*self.inner;
        tracing::warn!(repo = %id, reason, "repo is corrupt; queueing a re-clone");
        metrics::counter!(metric_names::REPO_CORRUPT_TOTAL).increment(1);
        if let Err(e) = inner.db.gitserver_repos().set_corrupted(id, reason).await {
            tracing::warn!(repo = %id, error = %e, "marking the repo corrupt failed");
        }
        if let Err(e) = inner
            .db
            .repo_update_jobs()
            .enqueue(id, RepoUpdatePriority::NORMAL, RepoUpdateReason::Scheduled)
            .await
        {
            tracing::warn!(repo = %id, error = %e, "queueing the re-clone failed");
        }
    }

    pub(crate) fn policy(&self) -> Policy {
        Policy::from_config(&self.inner.site.load().value)
    }
}

fn scheduler_mean(s: &FetchSchedule) -> Option<Duration> {
    s.mean_change_interval_secs
        .filter(|m| m.is_finite() && *m >= 0.0)
        .map(Duration::from_secs_f64)
}

/// Writes the latest progress line every [`PROGRESS_INTERVAL`] until `stop` fires.
async fn write_progress(
    node: RepoNode,
    repo: RepoId,
    latest: Arc<Mutex<Option<String>>>,
    stop: CancellationToken,
) {
    let mut last_written: Option<String> = None;
    let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let line = latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if line.is_some() && line != last_written {
            let text = line.clone().unwrap_or_default();
            if let Err(e) = node
                .inner
                .db
                .gitserver_repos()
                .set_cloning_progress(repo, &text)
                .await
            {
                tracing::debug!(repo = %repo, error = %e, "writing clone progress failed");
            }
            last_written = line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corruption_hints() {
        assert!(suggests_corruption(&GitError::Corrupt("x".into())));
        assert!(suggests_corruption(&GitError::Command {
            op: "fetch".into(),
            status: "exit status: 128".into(),
            stderr: "fatal: bad object refs/heads/main".into(),
        }));
        assert!(!suggests_corruption(&GitError::Command {
            op: "fetch".into(),
            status: "exit status: 128".into(),
            stderr: "fatal: Authentication failed".into(),
        }));
        assert_eq!(parse_reason("webhook"), RepoUpdateReason::Webhook);
        assert_eq!(parse_reason("bogus"), RepoUpdateReason::Scheduled);
    }
}
