//! Bulk operations on changesets: the pgqueue worker over `changeset_jobs`, run by the worker
//! role.
//!
//! A bulk operation is one job per changeset (sharing a `bulk_group`); the GraphQL mutations
//! check permissions and create them. Each job is carried out as the user who started it:
//! - `comment`, `merge` (`payload.squash`) and `close` call the code host through the
//!   changeset's [`ChangesetSource`](sourcegull_codehost::ChangesetSource) with that user's batch
//!   changes credential, else the site's (like the reconciler, which uses the last
//!   applier's); merge and close store the merge request the code host returned.
//! - `publish` (`payload.draft`) sets the changeset's UI publication state and queues it for
//!   the reconciler, which publishes it. Imported changesets and changesets whose spec sets
//!   `published` cannot be published from the UI.
//! - `detach` asks the reconciler to detach the changeset from the bulk change.
//! - `reenqueue` queues the changeset for the reconciler again (resetting its failures).
//!
//! Failures are recorded on the job (the bulk operation's errors); transient code host and
//! database errors are retried with backoff first.

use std::time::Duration;

use async_trait::async_trait;
use sourcegull_batches::store::{
    ChangesetJob, ChangesetJobStore, ChangesetJobType, ChangesetSpecStore, ChangesetStore,
    UiPublicationState,
};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_types::metric_names;

use crate::error::{ReconcileError, Result};
use crate::executor::Reconciler;

/// Counter of processed bulk operation jobs, by `type` and `outcome` (`ok`, `retry`,
/// `failed`).
pub const BULK_JOBS_METRIC: &str = metric_names::BATCHES_BULK_JOBS_TOTAL;

/// Bulk operation jobs one worker process handles at once.
pub const BULK_CONCURRENCY: usize = 5;

/// Carries out one bulk operation job.
pub async fn process_job(r: &Reconciler, job: &ChangesetJob) -> Result<()> {
    let store = ChangesetStore::new(r.db());
    let ch = store
        .get(job.changeset_id)
        .await?
        .ok_or_else(|| ReconcileError::Invalid("the changeset no longer exists".into()))?;
    let flag = |name: &str| job.payload.get(name).and_then(serde_json::Value::as_bool);
    match job.job_type {
        ChangesetJobType::Comment => {
            let body = job
                .payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if body.trim().is_empty() {
                return Err(ReconcileError::Invalid("the comment is empty".into()));
            }
            r.comment_as(&ch, job.user_id, body).await
        }
        ChangesetJobType::Merge => {
            r.merge_as(&ch, job.user_id, flag("squash").unwrap_or(false))
                .await
        }
        ChangesetJobType::Close => r.close_as(&ch, job.user_id).await,
        ChangesetJobType::Publish => {
            let spec_id = ch.current_spec_id.ok_or_else(|| {
                ReconcileError::Invalid("cannot publish an imported changeset".into())
            })?;
            let spec = ChangesetSpecStore::new(r.db())
                .get(spec_id)
                .await?
                .ok_or_else(|| {
                    ReconcileError::Invalid("the changeset's spec no longer exists".into())
                })?;
            if spec.published.is_some() {
                return Err(ReconcileError::Invalid(
                    "cannot publish a changeset that has a published value set in its \
                     changesetTemplate"
                        .into(),
                ));
            }
            let state = if flag("draft").unwrap_or(false) {
                UiPublicationState::Draft
            } else {
                UiPublicationState::Published
            };
            store.publish_from_ui(ch.id, state).await?;
            Ok(())
        }
        ChangesetJobType::Detach => {
            if store.request_detach(job.batch_change_id, &[ch.id]).await? == 0 {
                return Err(ReconcileError::Invalid(
                    "the changeset is not in the bulk change".into(),
                ));
            }
            Ok(())
        }
        ChangesetJobType::Reenqueue => {
            store.enqueue(&[ch.id]).await?;
            Ok(())
        }
    }
}

/// Runs bulk operation jobs.
pub struct BulkHandler {
    reconciler: Reconciler,
}

impl BulkHandler {
    pub fn new(reconciler: Reconciler) -> Self {
        Self { reconciler }
    }
}

#[async_trait]
impl Handler<ChangesetJob> for BulkHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<ChangesetJob>) -> Result<(), JobError> {
        let job = &record.payload;
        let kind = job.job_type.as_str();
        match process_job(&self.reconciler, job).await {
            Ok(()) => {
                metrics::counter!(BULK_JOBS_METRIC, "type" => kind, "outcome" => "ok").increment(1);
                Ok(())
            }
            Err(e) => {
                let retry = e.is_retryable();
                let outcome = if retry { "retry" } else { "failed" };
                metrics::counter!(BULK_JOBS_METRIC, "type" => kind, "outcome" => outcome)
                    .increment(1);
                tracing::info!(job = record.id, changeset = job.changeset_id, kind,
                    error = %e, retry, "bulk operation job failed");
                Err(e.into())
            }
        }
    }
}

/// The bulk operations worker and its stalled-job resetter, ready to run.
pub fn bulk_workers(
    reconciler: Reconciler,
    worker_id: &str,
) -> Result<(Worker<ChangesetJob>, Resetter<ChangesetJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<ChangesetJob> = Queue::new(
        reconciler.db().pool().clone(),
        ChangesetJobStore::queue_config(),
    )?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = BULK_CONCURRENCY;
    let worker = Worker::new(queue.clone(), BulkHandler::new(reconciler), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
