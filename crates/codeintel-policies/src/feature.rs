//! Data retention as a server feature: the upload expirer singleton.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::expirer;

/// The upload expirer ([`expirer::run_expirer`]), which applies data retention policies. Needs
/// the repository nodes.
pub struct RetentionFeature;

impl<S> Feature<S> for RetentionFeature {
    fn name(&self) -> &'static str {
        "codeintel-retention"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let Some(clients) = ctx.repo_clients.clone() else {
            return Ok(());
        };
        let db = ctx.db.clone();
        tasks.singleton(expirer::UPLOAD_EXPIRER, move |token, _fence| {
            expirer::run_expirer(
                db.clone(),
                clients.clone(),
                expirer::ExpirerConfig::default(),
                token,
            )
        });
        Ok(())
    }
}
