//! Permissions syncing as a server feature: the scheduler singleton and the sync workers.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};
use sourcegull_repos::Registry;

use crate::sync::PermsSyncer;
use crate::worker;

/// Permissions syncs: the scheduler ([`worker::run_scheduler`]) and the `permission_sync_jobs`
/// workers with their resetter ([`worker::perms_sync_workers`]).
pub struct PermsSyncFeature;

impl<S> Feature<S> for PermsSyncFeature {
    fn name(&self) -> &'static str {
        "perms-sync"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let syncer = PermsSyncer::new(
            ctx.db.clone(),
            ctx.keyring.clone(),
            Registry::default(),
            ctx.config.clone(),
        );
        {
            let (db, syncer, config) = (ctx.db.clone(), syncer.clone(), ctx.config.clone());
            tasks.singleton(worker::PERMS_SYNC_SCHEDULER, move |token, _fence| {
                worker::run_scheduler(db.clone(), syncer.clone(), config.clone(), token)
            });
        }
        let (w, resetter) = worker::perms_sync_workers(&ctx.db, syncer, &ctx.worker_id)?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        Ok(())
    }
}
