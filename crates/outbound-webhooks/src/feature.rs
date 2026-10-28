//! Outbound webhooks as a server feature: the delivery workers and the log janitor.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

/// The `outbound_webhook_jobs` workers with their resetter ([`crate::sender_workers`]) and the
/// log janitor singleton ([`crate::run_janitor`]).
pub struct OutboundWebhooksFeature;

impl<S> Feature<S> for OutboundWebhooksFeature {
    fn name(&self) -> &'static str {
        "outbound-webhooks"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let sender = crate::Sender::new(ctx.db.clone(), ctx.keyring.clone(), ctx.config.clone());
        let (w, resetter) = crate::sender_workers(
            sender,
            crate::store::OutboundWebhookStore::queue_config(),
            &ctx.worker_id,
        )?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        let (db, config) = (ctx.db.clone(), ctx.config.clone());
        tasks.singleton(crate::JANITOR, move |token, _fence| {
            crate::run_janitor(db.clone(), config.clone(), token)
        });
        Ok(())
    }
}
