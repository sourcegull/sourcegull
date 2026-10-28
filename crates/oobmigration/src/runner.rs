//! The out-of-band migration runner: a leader-elected singleton of the `worker` role
//! (lease [`OOB_MIGRATION_RUNNER`]) that runs every registered migration in the direction
//! its row asks for and records progress and errors.
//!
//! Each pass measures a migration's progress, runs one batch if there is work (`up` while
//! progress < 1, `down` while applied in reverse and progress > 0), re-measures and stores the
//! result. Passes repeat right away while some migration has work and every
//! [`IDLE_INTERVAL`] otherwise, so a later key rotation is picked up without a restart.
//! Batches and progress are written under the runner's lease [`Fence`], so a runner that lost
//! the lease neither migrates nor overwrites the progress its successor recorded.

use std::sync::Arc;
use std::time::Duration;

use sourcegull_db::{Db, DbError, Fence};
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::{Registered, Registry};

/// Lease name of the runner.
pub const OOB_MIGRATION_RUNNER: &str = "oob-migration-runner";
/// Pause between passes when no migration has work.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(30);
/// Pause between batches while some migration has work.
pub const BUSY_INTERVAL: Duration = Duration::from_millis(100);
/// Gauge of each migration's progress (label `id`).
pub const PROGRESS_METRIC: &str = metric_names::OOB_MIGRATION_PROGRESS;
/// Counter of failed batches (label `id`).
pub const ERRORS_METRIC: &str = metric_names::OOB_MIGRATION_ERRORS_TOTAL;

/// Runs the registry's migrations.
#[derive(Debug, Clone)]
pub struct Runner {
    db: Db,
    registry: Arc<Registry>,
    fence: Fence,
}

/// What one pass over a migration did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Nothing to do in its direction, or the batch failed (retried after a pause).
    Idle,
    /// A batch ran; more work may remain.
    Worked,
}

impl Runner {
    /// A runner of `registry` on `db`, not fenced (see [`Runner::with_fence`]).
    pub fn new(db: Db, registry: Arc<Registry>) -> Self {
        Self {
            db,
            registry,
            fence: Fence::none(OOB_MIGRATION_RUNNER),
        }
    }

    /// Writes progress only while `fence` is current.
    pub fn with_fence(mut self, fence: Fence) -> Self {
        self.fence = fence;
        self
    }

    /// Registers the migrations, then runs passes until `cancel` fires.
    pub async fn run(self, cancel: CancellationToken) {
        if let Err(e) = self.registry.sync(&self.db).await {
            tracing::warn!(error = %e, "registering out-of-band migrations failed");
        }
        loop {
            let busy = match sourcegull_db::record_run(OOB_MIGRATION_RUNNER, self.run_once()).await
            {
                Ok(busy) => busy,
                Err(e) => {
                    tracing::warn!(error = %e, "out-of-band migration pass failed");
                    false
                }
            };
            let pause = if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(pause) => {}
            }
        }
    }

    /// One pass over every migration; returns whether any of them did work.
    pub async fn run_once(&self) -> sourcegull_db::Result<bool> {
        let mut busy = false;
        for m in self.registry.iter() {
            busy |= self.step(m).await? == Step::Worked;
        }
        Ok(busy)
    }

    /// One pass over migration `m`.
    pub async fn step(&self, m: &Registered) -> sourcegull_db::Result<Step> {
        let store = self.db.oob_migrations();
        let id = m.descriptor.id;
        let Some(row) = store.get(id).await? else {
            self.registry.sync(&self.db).await?;
            return Ok(Step::Worked);
        };
        let label = id.to_string();
        if row.apply_reverse && !m.migrator.reversible() {
            // Nothing to undo: the old format stays readable.
            store.update_progress(id, 0.0, &self.fence).await?;
            metrics::gauge!(PROGRESS_METRIC, "id" => label).set(0.0);
            return Ok(Step::Idle);
        }
        let progress = match m.migrator.progress().await {
            Ok(p) => p,
            Err(e) => {
                self.record_error(m, &format!("measuring progress: {e:#}"))
                    .await?;
                return Ok(Step::Idle);
            }
        };
        let work = if row.apply_reverse {
            progress > 0.0
        } else {
            progress < 1.0
        };
        if !work {
            store.update_progress(id, progress, &self.fence).await?;
            metrics::gauge!(PROGRESS_METRIC, "id" => label).set(progress);
            return Ok(Step::Idle);
        }
        let result = if row.apply_reverse {
            m.migrator.down(&self.fence).await
        } else {
            m.migrator.up(&self.fence).await
        };
        let failed = result.is_err();
        if let Err(e) = result {
            // A runner that lost its lease stops; its successor runs the migration.
            if let Some(DbError::Fenced {
                name,
                epoch,
                current,
            }) = e.downcast_ref::<DbError>()
            {
                return Err(DbError::Fenced {
                    name: name.clone(),
                    epoch: *epoch,
                    current: *current,
                });
            }
            self.record_error(m, &format!("{e:#}")).await?;
        }
        let before = progress;
        let progress = m.migrator.progress().await.unwrap_or(progress);
        store.update_progress(id, progress, &self.fence).await?;
        metrics::gauge!(PROGRESS_METRIC, "id" => label).set(progress);
        tracing::debug!(
            id,
            progress,
            reverse = row.apply_reverse,
            "out-of-band migration batch"
        );
        // After a failure, wait for the idle interval instead of retrying right away. A batch
        // that moved nothing (e.g. applied in reverse with no data, where progress stays 1)
        // waits too, rather than spinning.
        #[allow(clippy::float_cmp)]
        let moved = progress != before;
        Ok(if failed || !moved {
            Step::Idle
        } else {
            Step::Worked
        })
    }

    async fn record_error(&self, m: &Registered, message: &str) -> sourcegull_db::Result<()> {
        let id = m.descriptor.id;
        tracing::warn!(id, error = message, "out-of-band migration failed");
        metrics::counter!(ERRORS_METRIC, "id" => id.to_string()).increment(1);
        self.db.oob_migrations().add_error(id, message).await
    }
}
