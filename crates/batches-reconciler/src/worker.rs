//! The reconciler queue worker: a pgqueue worker over the `changesets` table.
//!
//! Every worker process runs [`reconciler_workers`]: a [`Worker`] that dequeues `queued`
//! changesets and runs [`Reconciler::reconcile`] with a configurable concurrency
//! (`batchChanges.reconcilerConcurrency`), heartbeating them, and a [`Resetter`] that returns
//! changesets of workers that stopped heartbeating to the queue. Retryable failures leave the
//! changeset `ERRORED` and retry with backoff, others (and the last attempt) mark it
//! `FAILED`. Changesets in the `scheduled` state are not dequeued: rollout windows release
//! them to `queued` ([`crate::scheduler`]). A changeset the reconciler moved to `scheduled`
//! (its plan publishes, and rollout windows apply) is no longer `processing`, so completing
//! the job leaves it alone.

use std::time::Duration;

use async_trait::async_trait;
use sourcegull_batches::store::{Changeset, ChangesetStore};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_types::metric_names;

use crate::executor::Reconciler;

/// Counter of reconciled changesets, by `outcome` (`ok`, `scheduled`, `retry`, `failed`).
pub const RECONCILED_METRIC: &str = metric_names::BATCHES_CHANGESETS_RECONCILED_TOTAL;

/// Runs the reconciler on dequeued changesets.
pub struct ReconcileHandler {
    reconciler: Reconciler,
}

impl ReconcileHandler {
    pub fn new(reconciler: Reconciler) -> Self {
        Self { reconciler }
    }
}

#[async_trait]
impl Handler<Changeset> for ReconcileHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<Changeset>) -> Result<(), JobError> {
        match self.reconciler.reconcile(&record.payload).await {
            Ok(plan) => {
                let outcome = if plan.scheduled { "scheduled" } else { "ok" };
                metrics::counter!(RECONCILED_METRIC, "outcome" => outcome).increment(1);
                Ok(())
            }
            Err(e) => {
                let retry = e.is_retryable();
                metrics::counter!(RECONCILED_METRIC, "outcome" => if retry { "retry" } else { "failed" })
                    .increment(1);
                tracing::info!(changeset = record.id, error = %e, retry, "reconciling changeset failed");
                Err(e.into())
            }
        }
    }
}

/// The reconciler worker (handling `concurrency` changesets at once) and its stalled-row
/// resetter, ready to run.
pub fn reconciler_workers(
    reconciler: Reconciler,
    worker_id: &str,
    concurrency: usize,
) -> Result<(Worker<Changeset>, Resetter<Changeset>), sourcegull_pgqueue::Error> {
    let queue: Queue<Changeset> = Queue::new(
        reconciler.db().pool().clone(),
        ChangesetStore::queue_config(),
    )?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = concurrency;
    let worker = Worker::new(queue.clone(), ReconcileHandler::new(reconciler), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
