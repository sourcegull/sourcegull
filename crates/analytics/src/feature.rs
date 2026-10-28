//! Usage analytics as a server feature: the rollup singleton.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

/// Usage analytics rollups and event log retention ([`crate::run_rollup`]).
pub struct AnalyticsFeature;

impl<S> Feature<S> for AnalyticsFeature {
    fn name(&self) -> &'static str {
        "analytics"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let (db, config) = (ctx.db.clone(), ctx.config.clone());
        tasks.singleton(crate::ROLLUP, move |token, fence| {
            crate::run_rollup(db.clone(), config.clone(), fence, token)
        });
        Ok(())
    }
}
