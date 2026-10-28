//! Watches as a server feature: the trigger and action workers and the evaluator.

use std::sync::Arc;

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::{ActionSender, EVALUATOR, EvaluatorConfig};

/// The `cm_trigger_jobs` workers and the evaluator singleton (both need the repository nodes),
/// and the `cm_action_jobs` workers (email, and HTTP actions queued without encryption keys),
/// each queue with a resetter. The outbound webhook workers deliver the HTTP actions' channel
/// jobs.
pub struct WatchesFeature;

impl<S> Feature<S> for WatchesFeature {
    fn name(&self) -> &'static str {
        "watches"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        if let Some(clients) = ctx.repo_clients.clone() {
            let authz = sourcegull_authz::Checker::new(ctx.db.clone(), ctx.config.clone());
            let backend = sourcegull_services::search_backend(ctx.db.clone(), authz, clients);
            let (w, resetter) =
                crate::trigger_workers(backend.clone(), ctx.keyring.clone(), &ctx.worker_id)?;
            tasks.worker(|token| w.run(token));
            tasks.worker(|token| resetter.run(token));
            tasks.singleton(EVALUATOR, move |token, fence| {
                crate::run_evaluator(backend.clone(), EvaluatorConfig::default(), fence, token)
            });
        }
        let sender = ActionSender::new(
            &ctx.db,
            ctx.config.clone(),
            Arc::new(sourcegull_services::email::SmtpEmailSender),
        )
        .with_keyring(ctx.keyring.clone());
        let (w, resetter) = crate::action_workers(&ctx.db, sender, &ctx.worker_id)?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        Ok(())
    }
}
