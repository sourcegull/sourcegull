//! Bulk change spec resolution as a server feature: the resolution workers.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

/// The `batch_spec_resolution_jobs` workers with their resetter
/// ([`crate::worker::resolution_workers`]). Needs the repository nodes.
pub struct ResolverFeature;

impl<S> Feature<S> for ResolverFeature {
    fn name(&self) -> &'static str {
        "batches-resolver"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let Some(clients) = ctx.repo_clients.clone() else {
            return Ok(());
        };
        let (w, resetter) = crate::worker::resolution_workers(
            &ctx.db,
            ctx.config.clone(),
            clients,
            &ctx.worker_id,
        )?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        Ok(())
    }
}
