//! The bulk changes reconciler as a server feature: its queue workers and singletons.

use std::sync::Arc;

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};
use sourcegull_repos::Registry;

use crate::{
    DbSourceProvider, PREVIEWS_JANITOR, RETENTION_JANITOR, Reconciler, SCHEDULER, SYNCER,
    Scheduler, SystemClock,
};

/// The `changesets` workers (`batchChanges.reconcilerConcurrency` at a time) and the
/// `changeset_jobs` workers, each with a resetter ([`crate::reconciler_workers`],
/// [`crate::bulk_workers`]); the retention and previews janitors, the changeset syncer and the
/// rollout window scheduler singletons.
pub struct ReconcilerFeature;

impl<S> Feature<S> for ReconcilerFeature {
    fn name(&self) -> &'static str {
        "batches-reconciler"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let sources = Arc::new(DbSourceProvider::new(
            ctx.db.clone(),
            ctx.keyring.clone(),
            Registry::default(),
        ));
        let reconciler = Reconciler::new(
            ctx.db.clone(),
            ctx.keyring.clone(),
            sources,
            ctx.repo_clients.clone(),
            ctx.config.clone(),
        );
        let concurrency = ctx.config.load().value.batch_changes_reconciler_concurrency as usize;
        let (w, resetter) =
            crate::reconciler_workers(reconciler.clone(), &ctx.worker_id, concurrency)?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        let (w, resetter) = crate::bulk_workers(reconciler.clone(), &ctx.worker_id)?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        {
            let (db, config) = (ctx.db.clone(), ctx.config.clone());
            tasks.singleton(RETENTION_JANITOR, move |token, _fence| {
                crate::run_retention_janitor(db.clone(), config.clone(), token)
            });
        }
        {
            let db = ctx.db.clone();
            tasks.singleton(PREVIEWS_JANITOR, move |token, _fence| {
                crate::run_previews_janitor(db.clone(), token)
            });
        }
        tasks.singleton(SYNCER, move |token, fence| {
            crate::run_syncer(reconciler.clone(), fence, token)
        });
        let (db, config) = (ctx.db.clone(), ctx.config.clone());
        tasks.singleton(SCHEDULER, move |token, fence| {
            let scheduler =
                Scheduler::new(db.clone(), config.clone(), Arc::new(SystemClock)).with_fence(fence);
            crate::run_scheduler(scheduler, token)
        });
        Ok(())
    }
}
