//! Emitting events: [`Events`] queues one `outbound_webhook_jobs` row per event, but only
//! when some webhook subscribes to its type, so code paths that emit events cost one indexed
//! `EXISTS` query when no webhooks are configured.

use sourcegull_batches::store::{BatchChange, Changeset};
use sourcegull_db::{Db, DbError};
use std::sync::Arc;

use crate::store::OutboundWebhookStore;
use serde_json::Value;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;

use crate::payload;

/// Queues `payload` as an event of `event_type` (and `scope`, `None` for unscoped events) if a
/// webhook subscribes to it. Returns the job id, or `None` when nobody subscribes.
pub async fn enqueue_event(
    db: &Db,
    keyring: Option<Arc<Keyring>>,
    event_type: &str,
    scope: Option<&str>,
    payload: &Value,
) -> Result<Option<i64>, DbError> {
    let store = OutboundWebhookStore::new(db, keyring);
    if !store.has_subscribers(event_type, scope).await? {
        return Ok(None);
    }
    let body =
        serde_json::to_vec(payload).map_err(|_| DbError::Invalid("unserializable payload"))?;
    store.enqueue(event_type, scope, &body).await.map(Some)
}

/// Emits events for the bulk changes code (GraphQL mutations and the reconciler).
///
/// The `batch_change` and `changeset` methods never fail: an event that cannot be queued is
/// logged, and the operation that caused it goes on.
#[derive(Clone)]
pub struct Events {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    config: SiteConfigHandle,
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

impl Events {
    /// `keyring` encrypts payloads; `config` gives `externalURL` for links.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>, config: SiteConfigHandle) -> Self {
        Self {
            db,
            keyring,
            config,
        }
    }

    /// Queues `payload` as an unscoped event; see [`enqueue_event`].
    pub async fn enqueue(&self, event_type: &str, payload: &Value) -> Result<Option<i64>, DbError> {
        enqueue_event(&self.db, self.keyring.clone(), event_type, None, payload).await
    }

    async fn subscribed(&self, event_type: &str) -> bool {
        match OutboundWebhookStore::new(&self.db, None)
            .has_subscribers(event_type, None)
            .await
        {
            Ok(yes) => yes,
            Err(e) => {
                tracing::warn!(event_type, error = %e, "checking webhook subscriptions failed");
                false
            }
        }
    }

    fn report(event_type: &str, id: i64, result: Result<Option<i64>, DbError>) {
        match result {
            Ok(Some(job)) => tracing::debug!(event_type, id, job, "queued a webhook event"),
            Ok(None) => {}
            Err(e) => tracing::warn!(event_type, id, error = %e, "queuing a webhook event failed"),
        }
    }

    /// Emits a `batch_change:*` event (`batch_change:apply`, `:close`, `:delete`) with the
    /// bulk change's payload. Call it after the change is committed (for a delete, with the
    /// bulk change as it was).
    pub async fn batch_change(&self, event_type: &str, bc: &BatchChange) {
        if !self.subscribed(event_type).await {
            return;
        }
        let config = self.config.load();
        let result = match payload::batch_change(&self.db, &config.value, bc).await {
            Ok(p) => self.enqueue(event_type, &p).await,
            Err(e) => Err(e),
        };
        Self::report(event_type, bc.id, result);
    }

    /// Emits a `changeset:*` event with the changeset's payload.
    pub async fn changeset(&self, event_type: &str, ch: &Changeset) {
        if !self.subscribed(event_type).await {
            return;
        }
        let result = match payload::changeset(&self.db, ch).await {
            Ok(p) => self.enqueue(event_type, &p).await,
            Err(e) => Err(e),
        };
        Self::report(event_type, ch.id, result);
    }
}
