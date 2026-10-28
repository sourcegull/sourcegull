//! The `batch_spec_resolution_jobs` queue handler: resolves one spec's
//! workspaces per job, as the job's initiator, and stores them in one transaction. A spec
//! that cannot resolve (bad query, unknown repository or branch, ...) fails its job with the
//! reason as the failure message; database, repo node and search outages are retried.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sourcegull_batches::store::{BatchSpecStore, ResolutionJob};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_rpc::RepoClients;

use crate::{ResolveError, Resolver};

/// Resolutions run at once per worker process.
pub const WORKER_CONCURRENCY: usize = 5;

/// Processes resolution jobs with a [`Resolver`].
pub struct ResolutionHandler {
    resolver: Resolver,
}

impl ResolutionHandler {
    pub fn new(resolver: Resolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl Handler<ResolutionJob> for ResolutionHandler {
    async fn handle(
        &self,
        _ctx: JobContext,
        record: Record<ResolutionJob>,
    ) -> Result<(), JobError> {
        let job = record.payload;
        match self
            .resolver
            .resolve_and_store(job.batch_spec_id, job.initiator_id)
            .await
        {
            Ok(r) => {
                tracing::info!(
                    job = record.id,
                    batch_spec = job.batch_spec_id,
                    workspaces = r.workspaces.len(),
                    skipped = r.workspaces.iter().filter(|w| w.skipped).count(),
                    imports = r.imports.len(),
                    "resolved bulk change spec workspaces"
                );
                Ok(())
            }
            Err(ResolveError::Invalid(e)) => Err(JobError::fatal(e)),
            Err(ResolveError::Unavailable(e)) => Err(JobError::retry(e)),
        }
    }
}

/// The resolution queue worker and its stalled-job resetter, ready to run. Searches and
/// repository reads go to the repo nodes behind `clients`, with permissions from a checker
/// of its own (grants are at most `sourcegull_authz::USER_TTL` old).
pub fn resolution_workers(
    db: &Db,
    config: SiteConfigHandle,
    clients: Arc<RepoClients>,
    worker_id: &str,
) -> Result<(Worker<ResolutionJob>, Resetter<ResolutionJob>), sourcegull_pgqueue::Error> {
    let authz = sourcegull_authz::Checker::new(db.clone(), config);
    let resolver = Resolver::new(
        db.clone(),
        sourcegull_services::search_backend(db.clone(), authz, clients),
    );
    let queue: Queue<ResolutionJob> =
        Queue::new(db.pool().clone(), BatchSpecStore::resolution_queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = WORKER_CONCURRENCY;
    let worker = Worker::new(queue.clone(), ResolutionHandler::new(resolver), opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
