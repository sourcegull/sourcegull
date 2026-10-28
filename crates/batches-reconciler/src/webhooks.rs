//! Changeset webhooks: applies the merge request and pipeline events
//! [`sourcegull_repos::WebhookIngest`] parsed to the matching changesets right away.
//!
//! - **Merge request** (`ChangesetChanged`): the changeset is found by repository and merge
//!   request number. Its external state (open, draft, closed, merged), title, body, update
//!   time, the review state when the event decides it (fully approved, or no longer) and the
//!   changed host metadata (title, labels, …) are stored, and the event's timeline entry is
//!   upserted into `changeset_events` (keys starting with `hook:`; the next full sync
//!   replaces them with the code host's own ids). When the payload cannot describe the
//!   change (new commits, target branch, …) the changeset is synced from the code host at
//!   the syncer's next pass.
//! - **Pipeline** (`ChangesetChecksChanged`): the changesets of its merge request, or else
//!   of its branch. The pipeline's timeline entry is upserted (`pipeline:<id>`, as a sync
//!   keys it); the check state and head pipeline are updated unless the changeset already
//!   knows a newer pipeline.
//! - The computed UI state follows from the stored states (`changesets.computed_state`) and
//!   the next sync is scheduled with the webhook intervals ([`next_sync`]).
//!
//! Changesets that are not found (merge requests not opened by bulk changes, repositories
//! not mirrored) are ignored with a debug log.

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_batches::store::{
    Changeset, ChangesetHookUpdate, ChangesetStore, CheckState, ExternalState, NewChangesetEvent,
    ReviewState,
};
use sourcegull_codehost::{ChangesetEvent, ChangesetHook, ChecksHook, DomainEvent};
use sourcegull_db::{Db, DbError};
use sourcegull_repos::ChangesetHooks;
use sourcegull_types::metric_names;
use sourcegull_types::{ExternalService, Repo};

use crate::syncer::next_sync;

/// Counter of changesets updated from webhooks, by `event` (`merge_request`, `pipeline`).
pub const WEBHOOK_UPDATES_METRIC: &str = metric_names::BATCHES_CHANGESET_WEBHOOK_UPDATES_TOTAL;

/// Applies changeset webhooks to the database.
#[derive(Debug, Clone)]
pub struct ChangesetWebhooks {
    db: Db,
}

impl ChangesetWebhooks {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    async fn repo(
        &self,
        svc: &ExternalService,
        service_id: &str,
        external_id: &str,
    ) -> Result<Option<Repo>, DbError> {
        self.db
            .repos()
            .get_by_external_id(svc.kind.service_type(), service_id, external_id)
            .await
    }

    async fn merge_request(
        &self,
        repo: &Repo,
        number: i64,
        hook: &ChangesetHook,
    ) -> Result<(), DbError> {
        let store = ChangesetStore::new(&self.db);
        let Some(ch) = store
            .get_by_external_id(repo.id, &repo.external_service_type, &number.to_string())
            .await?
        else {
            tracing::debug!(repo = %repo.name, number, "merge request webhook for no changeset; ignored");
            return Ok(());
        };
        // The delivery is activity on the merge request (an approval does not always move
        // its update time).
        let now = Utc::now();
        let next = if hook.resync {
            now
        } else {
            next_sync(now, Some(now), true)
        };
        let update = ChangesetHookUpdate {
            // A deleted or read-only changeset keeps that state until a sync says otherwise.
            external_state: hook
                .state
                .and_then(|s| ExternalState::parse(s.as_str()))
                .filter(|_| {
                    !matches!(
                        ch.external_state,
                        Some(ExternalState::Deleted | ExternalState::ReadOnly)
                    )
                }),
            external_review_state: hook
                .review_state
                .and_then(|r| ReviewState::parse(r.as_str())),
            external_check_state: None,
            external_updated_at: hook.updated_at,
            title: hook.title.clone(),
            body: hook.body.clone(),
            metadata: hook.metadata.clone(),
            next_sync_at: Some(next),
        };
        store.update_from_hook(ch.id, &update).await?;
        store.upsert_events(ch.id, &events(&hook.events)).await?;
        metrics::counter!(WEBHOOK_UPDATES_METRIC, "event" => "merge_request").increment(1);
        tracing::info!(
            changeset = ch.id,
            action = hook.action.as_deref().unwrap_or(""),
            resync = hook.resync,
            "merge request webhook applied"
        );
        Ok(())
    }

    async fn checks(&self, ch: &Changeset, checks: &ChecksHook) -> Result<(), DbError> {
        let store = ChangesetStore::new(&self.db);
        store
            .upsert_events(ch.id, &events(std::slice::from_ref(&checks.event)))
            .await?;
        // Pipelines of earlier commits finish late but have lower ids than the head's.
        let newer_known = ch
            .metadata
            .get("headPipeline")
            .and_then(|p| p.get("id"))
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|id| id > checks.pipeline_id);
        if newer_known {
            tracing::debug!(
                changeset = ch.id,
                pipeline = checks.pipeline_id,
                "pipeline webhook for an older pipeline; event stored only"
            );
            return Ok(());
        }
        let now = Utc::now();
        let update = ChangesetHookUpdate {
            external_check_state: CheckState::parse(checks.state.as_str()),
            metadata: checks.metadata.clone(),
            next_sync_at: Some(next_sync(now, Some(now), true)),
            ..Default::default()
        };
        store.update_from_hook(ch.id, &update).await?;
        metrics::counter!(WEBHOOK_UPDATES_METRIC, "event" => "pipeline").increment(1);
        tracing::info!(
            changeset = ch.id,
            pipeline = checks.pipeline_id,
            state = checks.state.as_str(),
            "pipeline webhook applied"
        );
        Ok(())
    }
}

fn events(events: &[ChangesetEvent]) -> Vec<NewChangesetEvent> {
    events
        .iter()
        .map(|e| NewChangesetEvent {
            kind: e.kind.as_str().to_owned(),
            key: e.key.clone(),
            created_at: e.timestamp,
            metadata: e.metadata.clone(),
        })
        .collect()
}

#[async_trait]
impl ChangesetHooks for ChangesetWebhooks {
    async fn apply(
        &self,
        svc: &ExternalService,
        service_id: &str,
        event: &DomainEvent,
    ) -> Result<(), DbError> {
        match event {
            DomainEvent::ChangesetChanged {
                external_id,
                number,
                hook,
            } => {
                let Some(repo) = self.repo(svc, service_id, external_id).await? else {
                    tracing::debug!(%external_id, "merge request webhook for an unknown repo; ignored");
                    return Ok(());
                };
                self.merge_request(&repo, *number, hook).await
            }
            DomainEvent::ChangesetChecksChanged {
                external_id,
                number,
                branch,
                checks,
            } => {
                let Some(checks) = checks else {
                    return Ok(());
                };
                let Some(repo) = self.repo(svc, service_id, external_id).await? else {
                    tracing::debug!(%external_id, "pipeline webhook for an unknown repo; ignored");
                    return Ok(());
                };
                let store = ChangesetStore::new(&self.db);
                let changesets = match (number, branch) {
                    (Some(n), _) => store
                        .get_by_external_id(repo.id, &repo.external_service_type, &n.to_string())
                        .await?
                        .into_iter()
                        .collect(),
                    (None, Some(b)) => {
                        store
                            .by_branch(repo.id, &repo.external_service_type, b)
                            .await?
                    }
                    (None, None) => Vec::new(),
                };
                if changesets.is_empty() {
                    tracing::debug!(repo = %repo.name, ?number, ?branch, "pipeline webhook for no changeset; ignored");
                }
                for ch in &changesets {
                    self.checks(ch, checks).await?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
