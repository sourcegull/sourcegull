//! The repo node janitor: object store maintenance, corruption handling, stale temp cleanup,
//! removal of deleted repos, disk-pressure checks and reconciliation of `gitserver_repos` with what
//! is on disk.

use std::sync::atomic::Ordering;

use chrono::Utc;
use sourcegull_db::{RepoUpdatePriority, RepoUpdateReason};
use sourcegull_gitstore::janitor;
use sourcegull_types::metric_names;
use sourcegull_types::{CloneStatus, RepoId};

use super::RepoNode;

/// What one janitor pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JanitorReport {
    /// Repos found corrupt (marked and queued for a re-clone).
    pub corrupt: Vec<RepoId>,
    /// Repos removed from disk because they were deleted (or no longer exist).
    pub removed: Vec<RepoId>,
    /// `gitserver_repos` rows corrected to match the disk.
    pub reconciled: usize,
    /// Whether clones are paused for lack of disk space.
    pub clones_paused: bool,
}

#[derive(sqlx::FromRow)]
struct StatusRow {
    repo_id: RepoId,
    clone_status: String,
}

#[derive(sqlx::FromRow)]
struct LiveRow {
    id: RepoId,
    deleted_at: Option<chrono::DateTime<Utc>>,
}

impl RepoNode {
    /// One janitor pass. Errors of single repos are logged and do not stop the pass.
    pub async fn janitor_once(&self) -> anyhow::Result<JanitorReport> {
        let mut report = JanitorReport {
            clones_paused: self.check_disk(),
            ..Default::default()
        };
        report.reconciled = self.reconcile().await?;
        report.removed = self.remove_deleted().await?;
        let corrupt = self
            .inner
            .store
            .janitor_sweep(&self.inner.cfg.thresholds)
            .await?;
        for id in &corrupt {
            self.mark_corrupt(*id, "fsck failed during maintenance")
                .await;
        }
        report.corrupt = corrupt;
        Ok(report)
    }

    /// Updates the disk-pressure flag that pauses clones; returns it.
    fn check_disk(&self) -> bool {
        let inner = &*self.inner;
        let paused = match janitor::disk_usage(inner.store.layout().root()) {
            Ok(usage) => {
                metrics::gauge!(metric_names::REPO_DISK_FREE_BYTES).set(usage.available as f64);
                metrics::gauge!(metric_names::REPO_DISK_TOTAL_BYTES).set(usage.total as f64);
                let pressure = janitor::assess_disk(usage, &inner.cfg.disk);
                if pressure.pause_clones {
                    tracing::warn!(
                        free_ratio = usage.free_ratio(),
                        free_bytes = usage.available,
                        threshold_bytes = inner.cfg.disk.pause_clones_threshold(usage.total),
                        "repo node is low on disk space; pausing clones"
                    );
                }
                pressure.pause_clones
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading disk usage failed");
                false
            }
        };
        inner.clones_paused.store(paused, Ordering::Relaxed);
        paused
    }

    /// Fixes rows of this node's repos that disagree with the disk: `cloned` without a copy on
    /// disk (lost volume) becomes `not_cloned` and is queued for a clone; `cloning` with no
    /// clone running here (a crash mid-clone) takes the disk's state.
    async fn reconcile(&self) -> anyhow::Result<usize> {
        let inner = &*self.inner;
        let rows: Vec<StatusRow> = sqlx::query_as(
            "SELECT g.repo_id, g.clone_status FROM gitserver_repos g JOIN repo r ON r.id = g.repo_id
             WHERE r.shard = $1 AND r.deleted_at IS NULL AND g.clone_status IN ('cloned', 'cloning')",
        )
        .bind(&inner.me)
        .fetch_all(inner.db.pool())
        .await?;
        let gs = inner.db.gitserver_repos();
        let mut fixed = 0;
        for row in rows {
            let on_disk = inner.store.is_cloned(row.repo_id);
            match row.clone_status.as_str() {
                "cloned" if !on_disk => {
                    tracing::warn!(repo = %row.repo_id, "repo is missing on disk; queueing a clone");
                    gs.set_clone_status(row.repo_id, CloneStatus::NotCloned)
                        .await?;
                    inner
                        .db
                        .repo_update_jobs()
                        .enqueue(
                            row.repo_id,
                            RepoUpdatePriority::NORMAL,
                            RepoUpdateReason::Scheduled,
                        )
                        .await?;
                    fixed += 1;
                }
                "cloning" if !inner.store.is_cloning(row.repo_id) => {
                    let status = if on_disk {
                        CloneStatus::Cloned
                    } else {
                        CloneStatus::NotCloned
                    };
                    gs.set_clone_status(row.repo_id, status).await?;
                    fixed += 1;
                }
                _ => {}
            }
        }
        Ok(fixed)
    }

    /// Removes repos from disk whose row is gone, or that were deleted longer than the grace
    /// period ago. Repos assigned to another node are left alone (moving them is the
    /// rebalancer's job).
    async fn remove_deleted(&self) -> anyhow::Result<Vec<RepoId>> {
        let inner = &*self.inner;
        let on_disk: Vec<RepoId> = inner
            .store
            .layout()
            .list_repos()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if on_disk.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<LiveRow> =
            sqlx::query_as("SELECT id, deleted_at FROM repo WHERE id = ANY($1)")
                .bind(&on_disk)
                .fetch_all(inner.db.pool())
                .await?;
        let grace =
            chrono::Duration::from_std(inner.cfg.deleted_grace).unwrap_or(chrono::Duration::MAX);
        let cutoff = Utc::now().checked_sub_signed(grace);
        let mut removed = Vec::new();
        for id in on_disk {
            let remove = match rows.iter().find(|r| r.id == id) {
                None => true,
                Some(r) => match (r.deleted_at, cutoff) {
                    (Some(at), Some(cutoff)) => at <= cutoff,
                    _ => false,
                },
            };
            if !remove {
                continue;
            }
            match inner.store.delete(id).await {
                Ok(true) => {
                    tracing::info!(repo = %id, "removed deleted repo from disk");
                    self.queue_index(id, std::time::Duration::ZERO);
                    let _ = inner
                        .db
                        .gitserver_repos()
                        .set_clone_status(id, CloneStatus::NotCloned)
                        .await;
                    removed.push(id);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(repo = %id, error = %e, "removing a deleted repo failed"),
            }
        }
        Ok(removed)
    }

    /// Runs [`janitor_once`](Self::janitor_once) after the initial delay and then every
    /// janitor interval until `cancel` fires.
    pub(crate) async fn run_janitor(self, cancel: tokio_util::sync::CancellationToken) {
        let cfg = &self.inner.cfg;
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(cfg.janitor_initial_delay) => {}
        }
        let mut tick = tokio::time::interval(cfg.janitor_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let pass = self.janitor_once();
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                r = pass => r,
            };
            match result {
                Ok(r) => {
                    if !r.corrupt.is_empty() || !r.removed.is_empty() || r.reconciled > 0 {
                        tracing::info!(
                            corrupt = r.corrupt.len(),
                            removed = r.removed.len(),
                            reconciled = r.reconciled,
                            "repo janitor pass finished"
                        );
                    }
                }
                Err(e) => tracing::warn!(error = format!("{e:#}"), "repo janitor pass failed"),
            }
        }
    }
}
