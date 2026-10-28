//! Code insights as a server feature: the export routes, the queue workers, the enqueuer and the
//! janitor.

use axum::Router;
use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};
use sourcegull_services::AppState;

use crate::{Counter, ENQUEUER, Enqueuer, InsightStore, JANITOR};

/// The routes of [`crate::http::routes`]; the `insight_backfill_jobs` and
/// `insight_recording_jobs` workers with their resetters (`insights.query.worker.concurrency` at
/// a time) and the enqueuer singleton, both needing the repository nodes; the retention janitor
/// singleton.
pub struct InsightsFeature;

impl Feature<AppState> for InsightsFeature {
    fn name(&self) -> &'static str {
        "insights"
    }

    fn routes(&self) -> Option<Router<AppState>> {
        Some(crate::http::routes())
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        if let Some(clients) = ctx.repo_clients.clone() {
            let authz = sourcegull_authz::Checker::new(ctx.db.clone(), ctx.config.clone());
            let backend = sourcegull_services::search_backend(ctx.db.clone(), authz, clients);
            let counter = Counter::new(backend, Some(ctx.config.clone()));
            let concurrency = ctx.config.load().value.insights_query_worker_concurrency as usize;
            let workers = crate::workers(counter.clone(), &ctx.worker_id, concurrency)?;
            tasks.worker(|token| workers.run(token));
            let config = ctx.config.clone();
            tasks.singleton(ENQUEUER, move |token, fence| {
                let enqueuer =
                    Enqueuer::new(counter.clone(), Some(config.clone())).with_fence(fence);
                crate::run_enqueuer(enqueuer, token)
            });
        }
        let store = InsightStore::new(&ctx.db);
        let config = ctx.config.clone();
        tasks.singleton(JANITOR, move |token, _fence| {
            crate::run_janitor(store.clone(), config.clone(), token)
        });
        Ok(())
    }
}
