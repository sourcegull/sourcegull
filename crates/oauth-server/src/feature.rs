//! The OAuth authorization server as a server feature: its routes and the janitor singleton.

use axum::Router;
use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};
use sourcegull_services::AppState;

/// The authorization server's endpoints ([`crate::routes`]) and its janitor
/// ([`crate::janitor::run_janitor`]).
pub struct OAuthServerFeature;

impl Feature<AppState> for OAuthServerFeature {
    fn name(&self) -> &'static str {
        "oauth-server"
    }

    fn routes(&self) -> Option<Router<AppState>> {
        Some(crate::routes())
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let db = ctx.db.clone();
        tasks.singleton(crate::janitor::JANITOR, move |token, _fence| {
            crate::janitor::run_janitor(db.clone(), token)
        });
        Ok(())
    }
}
