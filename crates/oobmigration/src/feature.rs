//! Out-of-band migrations as a server feature: the runner singleton.

use std::sync::Arc;

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

/// The out-of-band migration runner ([`crate::Runner`]), e.g. re-encryption after a key rotation.
pub struct OobMigrationFeature;

impl<S> Feature<S> for OobMigrationFeature {
    fn name(&self) -> &'static str {
        "oobmigration"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let runner = crate::Runner::new(
            ctx.db.clone(),
            Arc::new(crate::Registry::sourcegull(
                ctx.db.clone(),
                ctx.keyring.clone(),
            )),
        );
        tasks.singleton(crate::OOB_MIGRATION_RUNNER, move |token, fence| {
            runner.clone().with_fence(fence).run(token)
        });
        Ok(())
    }
}
