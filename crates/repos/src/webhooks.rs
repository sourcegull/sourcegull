//! Webhook ingestion: finds the connection a delivery belongs to, verifies and parses
//! it with the code host's [`WebhookHandler`](sourcegull_codehost::WebhookHandler), applies the
//! resulting events to the database and logs the delivery (`webhook_logs`).
//!
//! Effects of events:
//! - [`DomainEvent::RefsUpdated`]: a `repo_ref_events` row per ref (source `webhook`) and a
//!   repo update request at [`RepoUpdatePriority::IMMEDIATE`] (coalesced per repo), which
//!   wakes the repo node through `NOTIFY queue_repo_update_jobs`. Unknown repos (not
//!   mirrored, or excluded) are ignored.
//! - [`DomainEvent::RepoChanged`]: a sync of the connection (reason `webhook`).
//! - [`DomainEvent::UserPermsChanged`]: a permissions sync of the Sourcegull user who owns the
//!   code host account (reason `webhook`), or else of the user with the same username (who
//!   the `identityProvider: username` sync matches). Unknown users are ignored.
//! - [`DomainEvent::RepoPermsChanged`]: a repo-centric permissions sync of the repo (reason
//!   `webhook`). Unknown repos are ignored.
//! - [`DomainEvent::ChangesetChanged`] and [`DomainEvent::ChangesetChecksChanged`]: handed
//!   to the [`ChangesetHooks`] (bulk changes, implemented by the reconciler crate);
//!   accepted and only logged without one.

use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;
use serde_json::{Value, json};
use sourcegull_codehost::{DomainEvent, WebhookError};
use sourcegull_crypto::Keyring;
use sourcegull_db::{
    AccountSpec, Db, DbError, NewWebhookLog, PermsSyncReason, PermsSyncSubject, RefEventSource,
    RefUpdate, RepoUpdatePriority, RepoUpdateReason, SyncReason,
};
use sourcegull_types::{ExternalService, ExternalServiceId, ExternalServiceKind};

use crate::Registry;

/// Largest body logged verbatim in `webhook_logs` (bytes); bigger bodies are truncated.
const MAX_LOGGED_BODY: usize = 64 * 1024;

/// Applies merge request and pipeline events to changesets (bulk changes).
#[async_trait]
pub trait ChangesetHooks: Send + Sync + std::fmt::Debug {
    /// Applies `event` (a `ChangesetChanged` or `ChangesetChecksChanged`) from connection
    /// `svc`, whose code host instance id is `service_id`. Unknown changesets are ignored.
    async fn apply(
        &self,
        svc: &ExternalService,
        service_id: &str,
        event: &DomainEvent,
    ) -> Result<(), DbError>;
}

/// Which endpoint a delivery arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookTarget<'a> {
    /// `/.api/gitlab-webhooks`: the connection is found by matching the secret against every
    /// GitLab connection (optionally narrowed by `?externalServiceID=`).
    GitLab {
        /// The connection the caller named, if any.
        external_service: Option<ExternalServiceId>,
    },
    /// `/.api/webhooks/{uuid}`: the connection with this webhook UUID.
    Uuid(&'a str),
}

/// The result of handling a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookOutcome {
    /// The HTTP status to answer with.
    pub status: u16,
    /// A short message for the response body.
    pub message: String,
    /// The connection the delivery was attributed to.
    pub external_service: Option<ExternalServiceId>,
    /// The parsed events (empty unless accepted).
    pub events: Vec<DomainEvent>,
}

impl WebhookOutcome {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            external_service: None,
            events: Vec::new(),
        }
    }
}

/// Handles incoming webhook deliveries.
#[derive(Debug, Clone)]
pub struct WebhookIngest {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    registry: Registry,
    changesets: Option<Arc<dyn ChangesetHooks>>,
}

impl WebhookIngest {
    /// An ingester reading connection configs with `keyring`.
    pub fn new(db: Db, keyring: Option<Arc<Keyring>>, registry: Registry) -> Self {
        Self {
            db,
            keyring,
            registry,
            changesets: None,
        }
    }

    /// Applies changeset events with `hooks`.
    #[must_use]
    pub fn with_changeset_hooks(mut self, hooks: Option<Arc<dyn ChangesetHooks>>) -> Self {
        self.changesets = hooks;
        self
    }

    /// Handles one delivery and logs it. Never fails: problems become the outcome's status.
    pub async fn handle(
        &self,
        target: WebhookTarget<'_>,
        headers: &HeaderMap,
        body: &[u8],
    ) -> WebhookOutcome {
        let target_path = match target {
            WebhookTarget::GitLab {
                external_service: Some(id),
            } => format!("/.api/gitlab-webhooks?externalServiceID={id}"),
            WebhookTarget::GitLab { .. } => "/.api/gitlab-webhooks".to_owned(),
            WebhookTarget::Uuid(uuid) => format!("/.api/webhooks/{uuid}"),
        };
        let outcome = match self.process(target, headers, body).await {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(error = %e, "webhook processing failed");
                WebhookOutcome::new(500, "internal error")
            }
        };
        let log = NewWebhookLog {
            external_service_id: outcome.external_service,
            event: headers
                .get(sourcegull_codehost_gitlab::webhooks::EVENT_HEADER)
                .or_else(|| headers.get(sourcegull_codehost_gitea::webhooks::EVENT_HEADER))
                .or_else(|| headers.get(sourcegull_codehost_github::webhooks::EVENT_HEADER))
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            status_code: i32::from(outcome.status),
            request: logged_request(&target_path, headers, body),
            error: (outcome.status >= 400).then(|| outcome.message.clone()),
        };
        if let Err(e) = self.db.webhook_logs().insert(&log).await {
            tracing::warn!(error = %e, "logging webhook delivery failed");
        }
        outcome
    }

    async fn process(
        &self,
        target: WebhookTarget<'_>,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<WebhookOutcome, DbError> {
        let store = self.db.external_services(self.keyring.clone());
        let candidates: Vec<(ExternalService, String)> = match target {
            WebhookTarget::Uuid(uuid) => {
                let Some(svc) = store.get_by_webhook_uuid(uuid).await? else {
                    return Ok(WebhookOutcome::new(404, "unknown webhook"));
                };
                let config = store.config(svc.id).await?;
                vec![(svc, config)]
            }
            WebhookTarget::GitLab { external_service } => store
                .list_with_config(ExternalServiceKind::GitLab)
                .await?
                .into_iter()
                .filter(|(svc, _)| external_service.is_none_or(|id| svc.id == id))
                .collect(),
        };

        let mut matched = None;
        for (svc, config) in candidates {
            let handler = match self.registry.webhook_handler(svc.kind, &config) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(external_service = svc.id.get(), error = %e, "unusable connection config");
                    continue;
                }
            };
            if handler.verify(headers, body).is_ok() {
                matched = Some((svc, config, handler));
                break;
            }
        }
        let Some((svc, config, handler)) = matched else {
            return Ok(WebhookOutcome::new(
                401,
                "invalid or missing webhook secret",
            ));
        };
        let mut outcome = WebhookOutcome::new(200, "ok");
        outcome.external_service = Some(svc.id);
        let events = match handler.parse(headers, body) {
            Ok(events) => events,
            Err(WebhookError::Malformed(m)) => {
                outcome.status = 400;
                outcome.message = format!("malformed webhook: {m}");
                return Ok(outcome);
            }
            Err(WebhookError::Unauthorized) => {
                outcome.status = 401;
                outcome.message = "invalid or missing webhook secret".into();
                return Ok(outcome);
            }
        };
        let service_id = match self.registry.service_id(svc.kind, &config) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(external_service = svc.id.get(), error = %e, "unusable connection config");
                outcome.status = 500;
                outcome.message = "connection config is unusable".into();
                return Ok(outcome);
            }
        };
        for event in &events {
            self.apply(&svc, &service_id, event).await?;
        }
        outcome.events = events;
        Ok(outcome)
    }

    async fn apply(
        &self,
        svc: &ExternalService,
        service_id: &str,
        event: &DomainEvent,
    ) -> Result<(), DbError> {
        match event {
            DomainEvent::RefsUpdated { external_id, refs } => {
                let repo = self
                    .db
                    .repos()
                    .get_by_external_id(svc.kind.service_type(), service_id, external_id)
                    .await?;
                let Some(repo) = repo else {
                    tracing::debug!(external_service = svc.id.get(), %external_id, "push for an unknown repo; ignored");
                    return Ok(());
                };
                let updates: Vec<RefUpdate> = refs
                    .iter()
                    .map(|r| RefUpdate {
                        name: r.name.clone(),
                        old_oid: r.old_oid.clone(),
                        new_oid: r.new_oid.clone(),
                    })
                    .collect();
                self.db
                    .ref_events()
                    .record(repo.id, &updates, RefEventSource::Webhook)
                    .await?;
                self.db
                    .repo_update_jobs()
                    .enqueue(
                        repo.id,
                        RepoUpdatePriority::IMMEDIATE,
                        RepoUpdateReason::Webhook,
                    )
                    .await?;
                tracing::info!(repo = %repo.name, "push webhook: fetch requested");
            }
            DomainEvent::RepoChanged { .. } => {
                self.db
                    .sync_jobs()
                    .enqueue(svc.id, SyncReason::Webhook)
                    .await?;
            }
            DomainEvent::UserPermsChanged {
                account_id,
                username,
            } => {
                let spec = AccountSpec {
                    service_type: svc.kind.service_type(),
                    service_id,
                    account_id,
                };
                let mut user = self.db.external_accounts(None).user_for(spec).await?;
                if user.is_none()
                    && let Some(name) = username
                {
                    user = self.db.users().get_by_username(name).await?.map(|u| u.id);
                }
                let Some(user) = user else {
                    tracing::debug!(external_service = svc.id.get(), %account_id, "membership change for an unknown user; ignored");
                    return Ok(());
                };
                self.db
                    .perms_sync_jobs()
                    .enqueue(PermsSyncSubject::User(user), PermsSyncReason::Webhook, None)
                    .await?;
                tracing::info!(
                    user = user.get(),
                    "membership webhook: permissions sync requested"
                );
            }
            DomainEvent::RepoPermsChanged { external_id } => {
                let repo = self
                    .db
                    .repos()
                    .get_by_external_id(svc.kind.service_type(), service_id, external_id)
                    .await?;
                let Some(repo) = repo else {
                    tracing::debug!(external_service = svc.id.get(), %external_id, "access change for an unknown repo; ignored");
                    return Ok(());
                };
                self.db
                    .perms_sync_jobs()
                    .enqueue(
                        PermsSyncSubject::Repo(repo.id),
                        PermsSyncReason::Webhook,
                        None,
                    )
                    .await?;
                tracing::info!(repo = %repo.name, "access webhook: permissions sync requested");
            }
            DomainEvent::ChangesetChanged { .. } | DomainEvent::ChangesetChecksChanged { .. } => {
                match &self.changesets {
                    Some(hooks) => hooks.apply(svc, service_id, event).await?,
                    None => {
                        tracing::debug!(
                            external_service = svc.id.get(),
                            "changeset webhook without a handler; ignored"
                        );
                    }
                }
            }
            DomainEvent::Ignored { .. } => {}
        }
        Ok(())
    }
}

/// What `webhook_logs.request` keeps: the method and path, a few headers (never the secret)
/// and the body.
fn logged_request(path: &str, headers: &HeaderMap, body: &[u8]) -> Value {
    let keep = [
        "x-gitlab-event",
        "x-gitlab-instance",
        "x-gitlab-event-uuid",
        "user-agent",
        "content-type",
    ];
    let headers: serde_json::Map<String, Value> = keep
        .iter()
        .filter_map(|k| {
            headers
                .get(*k)
                .and_then(|v| v.to_str().ok())
                .map(|v| ((*k).to_owned(), Value::String(v.to_owned())))
        })
        .collect();
    let body = if body.len() > MAX_LOGGED_BODY {
        Value::String(format!(
            "{}… (truncated, {} bytes)",
            String::from_utf8_lossy(&body[..MAX_LOGGED_BODY]),
            body.len()
        ))
    } else {
        serde_json::from_slice(body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
    };
    json!({ "method": "POST", "url": path, "headers": headers, "body": body })
}
