//! Code Ownership as a server feature: the signal indexers, their scheduler and the team sync.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::{jobs, teamsync};

/// The `own_background_jobs` workers with their resetter and their scheduler singleton (both
/// need the repository nodes), and the team sync singleton.
pub struct OwnFeature;

impl<S> Feature<S> for OwnFeature {
    fn name(&self) -> &'static str {
        "own"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        if let Some(clients) = ctx.repo_clients.clone() {
            let indexer = jobs::Indexer::new(ctx.db.clone(), clients);
            let (w, resetter) = jobs::workers(indexer, &ctx.worker_id)?;
            tasks.worker(|token| w.run(token));
            tasks.worker(|token| resetter.run(token));
            let db = ctx.db.clone();
            tasks.singleton(jobs::SCHEDULER, move |token, _fence| {
                jobs::run_scheduler(db.clone(), Default::default(), token)
            });
        }
        let (db, keyring, config) = (ctx.db.clone(), ctx.keyring.clone(), ctx.config.clone());
        tasks.singleton(teamsync::TEAM_SYNC, move |token, fence| {
            teamsync::run_team_sync(db.clone(), keyring.clone(), config.clone(), fence, token)
        });
        Ok(())
    }
}
