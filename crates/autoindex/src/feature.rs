//! Auto-indexing as a server feature: the scheduler singleton.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::scheduler;

/// The auto-indexing scheduler ([`scheduler::run_scheduler`]). Needs the repository nodes.
pub struct AutoIndexFeature;

impl<S> Feature<S> for AutoIndexFeature {
    fn name(&self) -> &'static str {
        "autoindex"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let Some(clients) = ctx.repo_clients.clone() else {
            return Ok(());
        };
        let indexer = crate::AutoIndexer::new(ctx.db.clone(), clients, ctx.config.clone());
        tasks.singleton(scheduler::AUTOINDEX_SCHEDULER, move |token, _fence| {
            scheduler::run_scheduler(
                indexer.clone(),
                scheduler::SchedulerConfig::default(),
                token,
            )
        });
        Ok(())
    }
}
