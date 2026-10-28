//! Search jobs as a server feature: the download routes, the queue workers and the janitor.

use axum::Router;
use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};
use sourcegull_services::AppState;

use crate::{SearchJobStore, janitor, worker};

/// The download routes ([`crate::http::routes`]); the `exhaustive_search_*` workers with their
/// resetters ([`worker::search_job_workers`], needs the repository nodes); the janitor singleton
/// ([`janitor::run_janitor`]).
pub struct SearchJobsFeature;

impl Feature<AppState> for SearchJobsFeature {
    fn name(&self) -> &'static str {
        "searchjobs"
    }

    fn routes(&self) -> Option<Router<AppState>> {
        Some(crate::http::routes())
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        if let Some(clients) = ctx.repo_clients.clone() {
            let runner = worker::SearchJobRunner::new(
                ctx.db.clone(),
                ctx.config.clone(),
                clients,
                ctx.blobs.clone(),
            );
            let workers = worker::search_job_workers(runner, &ctx.worker_id)?;
            tasks.worker(|token| workers.run(token));
        }
        let store = SearchJobStore::new(ctx.db.pool().clone());
        let (blobs, config) = (ctx.blobs.clone(), ctx.config.clone());
        tasks.singleton(janitor::JANITOR, move |token, _fence| {
            janitor::run_janitor(store.clone(), blobs.clone(), config.clone(), token)
        });
        Ok(())
    }
}
