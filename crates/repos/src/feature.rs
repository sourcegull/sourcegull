//! Repository discovery as a server feature: the sync scheduler singleton and the sync workers.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::{Registry, Syncer, worker};

/// Code host connection syncs: the scheduler ([`worker::run_scheduler`]) and the
/// `external_service_sync_jobs` workers with their resetter ([`worker::sync_workers`]).
pub struct ReposFeature;

impl<S> Feature<S> for ReposFeature {
    fn name(&self) -> &'static str {
        "repos"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let db = ctx.db.clone();
        tasks.singleton(worker::SYNC_SCHEDULER, move |token, _fence| {
            worker::run_scheduler(db.clone(), token)
        });
        let syncer = Syncer::new(
            ctx.db.clone(),
            ctx.keyring.clone(),
            Registry::default(),
            ctx.config.clone(),
        );
        let (w, resetter) = worker::sync_workers(&ctx.db, syncer, &ctx.worker_id)?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        Ok(())
    }
}
