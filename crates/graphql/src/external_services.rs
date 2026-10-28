//! Code host connections: the `ExternalService` type (with `checkConnection`
//! and the `syncJobs` history), `externalServices`, `addExternalService`,
//! `updateExternalService`, `deleteExternalService`, `syncExternalService` and
//! `cancelExternalServiceSync`; plus `validateExternalServiceConfig` (live
//! validation for the editor), `checkExternalServiceConfig` (a connection check before
//! saving) and `regenerateExternalServiceWebhookSecret`. Site admins only.
//!
//! Configurations are validated with the code host's schema and rules (errors reject the write
//! with `VALIDATION`), stored encrypted, and returned with secrets replaced by `REDACTED`.
//! Sending `REDACTED` back on update keeps the stored secret. After a write the connection is
//! checked against the code host (token scopes, reachability); problems are reported in
//! `warning` without rejecting the write.

use std::time::Duration;

use async_graphql::{Context, Enum, ID, InputObject, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_codehost::CheckReport;
use sourcegull_config::ConfigError;
use sourcegull_db::{ExternalServiceUpdate, NewExternalService, SyncJobRecord, SyncReason};
use sourcegull_services::relay::{EXTERNAL_SERVICE_KIND, marshal_id, unmarshal_int_id};
use sourcegull_types::{
    ExternalService, ExternalServiceId, ExternalServiceKind, ExternalServiceSyncJobId,
};

use crate::EmptyResponse;
use crate::audit::{self, events};
use crate::context::{
    Services, codes, db_error, error, page_size, require_site_admin, services, to_int,
};
use crate::declared;
use crate::pagination::{PageInfo, decode_cursor};

/// Parses an `ExternalServiceSyncJob` node ID.
pub fn parse_sync_job_id(id: &str) -> Result<ExternalServiceSyncJobId> {
    unmarshal_int_id(SYNC_JOB_KIND, id)
        .map(ExternalServiceSyncJobId)
        .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid sync job ID {id:?}.")))
}

/// How long the post-save connection check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Node kind of sync job IDs.
pub const SYNC_JOB_KIND: &str = "ExternalServiceSyncJob";

pub use sourcegull_graphql_core::codehost::ExternalServiceKindGql;

/// Parses an `ExternalService` node ID.
pub fn parse_external_service_id(id: &str) -> Result<ExternalServiceId> {
    unmarshal_int_id(EXTERNAL_SERVICE_KIND, id)
        .map(ExternalServiceId)
        .ok_or_else(|| {
            error(
                codes::INVALID_INPUT,
                format!("Invalid external service ID {id:?}."),
            )
        })
}

/// A code host connection.
#[derive(Debug, Clone)]
pub struct ExternalServiceNode {
    pub service: ExternalService,
    /// Problems found by the connection check after a write.
    pub warning: Option<String>,
}

impl ExternalServiceNode {
    /// A node without a warning.
    pub fn new(service: ExternalService) -> Self {
        Self {
            service,
            warning: None,
        }
    }

    /// The decrypted configuration.
    async fn stored_config(&self, svc: &Services) -> Result<String> {
        svc.db
            .external_services(svc.keyring.clone())
            .config(self.service.id)
            .await
            .map_err(db_error)
    }
}

/// A code host connection's request budget: a token bucket refilled with `limit` tokens every
/// `interval` seconds.
#[derive(SimpleObject)]
pub struct RateLimiterState {
    /// The bucket size: the most requests that can be sent at once.
    pub burst: i32,
    /// The tokens in the bucket now.
    pub current_capacity: i32,
    /// The limiter is disabled (`rateLimit.enabled: false`).
    pub infinite: bool,
    /// The seconds it takes to refill `limit` tokens.
    pub interval: i32,
    /// When the bucket was last refilled.
    pub last_replenishment: DateTime<Utc>,
    /// The tokens refilled every `interval` (`rateLimit.requestsPerHour`).
    pub limit: i32,
    /// Until when the code host asked every client to back off (`Retry-After`, or
    /// `RateLimit-Remaining: 0` until `RateLimit-Reset`), if it did.
    pub paused_until: Option<DateTime<Utc>>,
}

impl From<sourcegull_codehost::RateLimiterState> for RateLimiterState {
    fn from(s: sourcegull_codehost::RateLimiterState) -> Self {
        let int = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
        Self {
            burst: int(s.burst),
            current_capacity: int(s.current_capacity),
            infinite: s.infinite,
            interval: int(s.interval),
            last_replenishment: s.last_replenishment,
            limit: int(s.limit),
            paused_until: s.paused_until,
        }
    }
}

/// The outcome of `checkConnection`.
#[derive(Union)]
pub enum ExternalServiceAvailability {
    Available(ExternalServiceAvailable),
    Unavailable(ExternalServiceUnavailable),
    Unknown(ExternalServiceAvailabilityUnknown),
}

/// The code host answered and accepted the connection's credentials.
#[derive(SimpleObject)]
pub struct ExternalServiceAvailable {
    /// When the check ran.
    pub last_checked_at: DateTime<Utc>,
}

/// The code host could not be reached or rejected the connection.
#[derive(SimpleObject)]
pub struct ExternalServiceUnavailable {
    /// Why the check failed.
    pub suspected_reason: String,
}

/// The connection's kind has no check.
#[derive(SimpleObject)]
pub struct ExternalServiceAvailabilityUnknown {
    /// Why availability is unknown.
    pub implementation_note: String,
}

#[Object(name = "ExternalService")]
impl ExternalServiceNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        ID(marshal_id(EXTERNAL_SERVICE_KIND, self.service.id.get()))
    }

    /// The code host kind.
    async fn kind(&self) -> ExternalServiceKindGql {
        self.service.kind.into()
    }

    /// The name shown in the UI.
    async fn display_name(&self) -> &str {
        &self.service.display_name
    }

    /// The normalized URL of the code host instance.
    async fn url(&self, ctx: &Context<'_>) -> Result<String> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        Ok(svc
            .registry
            .service_id(self.service.kind, &text)
            .unwrap_or_default())
    }

    /// The JSONC configuration with secrets replaced by `REDACTED`.
    async fn config(&self, ctx: &Context<'_>) -> Result<String> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        svc.registry.redact(self.service.kind, &text).map_err(|e| {
            error(
                codes::INTERNAL,
                format!("The stored configuration is invalid: {e}"),
            )
        })
    }

    /// The resources file entry that manages the connection (`connections[<key>]`), or
    /// `connections` when the file manages every connection; such connections are read-only.
    async fn managed_by(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        let svc = services(ctx)?;
        declared::managed_by(
            svc,
            declared::kinds::CONNECTIONS,
            &self.service.id.0.to_string(),
        )
        .await
    }

    /// When the connection was created.
    async fn created_at(&self) -> DateTime<Utc> {
        self.service.created_at
    }

    /// When the connection was last changed.
    async fn updated_at(&self) -> DateTime<Utc> {
        self.service.updated_at
    }

    /// The number of repositories the connection mirrors.
    async fn repo_count(&self, ctx: &Context<'_>) -> Result<i32> {
        let n = services(ctx)?
            .db
            .external_services(None)
            .repo_count(self.service.id)
            .await
            .map_err(db_error)?;
        Ok(to_int(n))
    }

    /// The URL to configure as this connection's webhook on the code host (relative when
    /// `externalURL` is not set).
    #[graphql(name = "webhookURL")]
    async fn webhook_url(&self, ctx: &Context<'_>) -> Result<String> {
        let base = services(ctx)?
            .config
            .load()
            .value
            .external_url
            .clone()
            .unwrap_or_default();
        Ok(format!(
            "{}/.api/webhooks/{}",
            base.trim_end_matches('/'),
            self.service.webhook_uuid
        ))
    }

    /// The webhook secret configured on the code host (the first of the configuration's
    /// `webhooks`), masked: `…` and its last 4 characters. GitLab sends it as
    /// `X-Gitlab-Token`, GitHub and Gitea sign deliveries with it. Null when none is
    /// configured. The secret itself is returned only by
    /// `regenerateExternalServiceWebhookSecret`; the configuration shows it as `REDACTED`.
    async fn webhook_secret(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        Ok(svc
            .registry
            .webhook_secret(self.service.kind, &text)
            .map(|s| mask_tail(&s)))
    }

    /// Webhook deliveries matched to this connection, newest first.
    async fn webhook_logs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        only_errors: Option<bool>,
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<crate::admin_logs::WebhookLogConnection> {
        crate::admin_logs::webhook_logs(
            ctx,
            sourcegull_db::WebhookLogFilter {
                service: Some(self.service.id),
                only_errors: only_errors.unwrap_or(false),
                since,
                until,
                ..Default::default()
            },
            first,
            after,
        )
        .await
    }

    /// Checks now that the code host is reachable and accepts the connection's token.
    async fn check_connection(&self, ctx: &Context<'_>) -> Result<ExternalServiceAvailability> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        let problems = check(svc, self.service.kind, &text).await;
        Ok(match warning(problems) {
            None => ExternalServiceAvailability::Available(ExternalServiceAvailable {
                last_checked_at: Utc::now(),
            }),
            Some(reason) => ExternalServiceAvailability::Unavailable(ExternalServiceUnavailable {
                suspected_reason: reason,
            }),
        })
    }

    /// Whether `checkConnection` can check this kind of connection.
    async fn has_connection_check(&self) -> bool {
        true
    }

    /// The current state of the connection's request budget (`rateLimit`), shared by
    /// every process when the Postgres rate limiter is in use.
    async fn rate_limiter_state(&self, ctx: &Context<'_>) -> Result<Option<RateLimiterState>> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        match svc
            .registry
            .for_connection(self.service.id.get())
            .rate_limiter_state(self.service.kind, &text)
            .await
        {
            Ok(state) => Ok(Some(state.into())),
            Err(e) => {
                tracing::warn!(error = %e, external_service = self.service.id.get(), "reading the rate limiter state failed");
                Ok(None)
            }
        }
    }

    /// Whether the configuration supports `exclude`.
    async fn supports_repo_exclusion(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(services(ctx)?
            .registry
            .supports_repo_exclusion(self.service.kind))
    }

    /// Whether the connection has no `authorization` block, so its repositories are readable
    /// by every user.
    async fn unrestricted(&self, ctx: &Context<'_>) -> Result<bool> {
        let svc = services(ctx)?;
        let text = self.stored_config(svc).await?;
        Ok(svc.registry.unrestricted(self.service.kind, &text))
    }

    /// Problems found when checking the connection after the last save, if any.
    async fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// When the last sync finished.
    async fn last_sync_at(&self) -> Option<DateTime<Utc>> {
        self.service.last_sync_at
    }

    /// When the next scheduled sync is due.
    async fn next_sync_at(&self) -> Option<DateTime<Utc>> {
        self.service.next_sync_at
    }

    /// The error of the last sync, if it failed.
    async fn last_sync_error(&self) -> Option<&str> {
        self.service.last_sync_error.as_deref()
    }

    /// Recent sync runs, newest first.
    async fn sync_jobs(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<ExternalServiceSyncJobConnection> {
        let store = services(ctx)?.db.sync_jobs();
        let limit = page_size(first, 10, 100);
        let offset = decode_cursor(after.as_deref())?;
        let jobs = store
            .list(self.service.id, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let total = store.count(self.service.id).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, jobs.len());
        Ok(ExternalServiceSyncJobConnection {
            total_count: to_int(total),
            page_info,
            nodes: jobs
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(SyncJobNode)
                .collect(),
        })
    }
}

/// The state of a sync run.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExternalServiceSyncJobState {
    Queued,
    Processing,
    Errored,
    Failed,
    Completed,
    /// Cancellation was requested; the worker stops at its next heartbeat.
    Canceling,
    Canceled,
}

/// One sync run of a connection.
pub struct SyncJobNode(pub SyncJobRecord);

#[Object(name = "ExternalServiceSyncJob")]
impl SyncJobNode {
    /// The opaque node ID.
    async fn id(&self) -> ID {
        ID(marshal_id(SYNC_JOB_KIND, self.0.id.get()))
    }

    /// The run's state.
    async fn state(&self) -> ExternalServiceSyncJobState {
        match self.0.state.as_str() {
            "queued" => ExternalServiceSyncJobState::Queued,
            "processing" if self.0.cancel => ExternalServiceSyncJobState::Canceling,
            "processing" => ExternalServiceSyncJobState::Processing,
            "errored" => ExternalServiceSyncJobState::Errored,
            "failed" => ExternalServiceSyncJobState::Failed,
            "canceled" => ExternalServiceSyncJobState::Canceled,
            _ => ExternalServiceSyncJobState::Completed,
        }
    }

    /// Why the run was queued (`scheduled`, `manual`, `config_changed`, `webhook`).
    async fn reason(&self) -> &str {
        &self.0.reason
    }

    /// Why the run failed.
    async fn failure_message(&self) -> Option<&str> {
        self.0.failure_message.as_deref()
    }

    /// Problems that did not fail the run (e.g. projects where the webhook could not be
    /// registered).
    async fn warnings(&self) -> &[String] {
        &self.0.warnings
    }

    /// When the run was queued.
    async fn queued_at(&self) -> DateTime<Utc> {
        self.0.queued_at
    }

    /// When the run started.
    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.0.started_at
    }

    /// When the run finished.
    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.0.finished_at
    }

    /// Whether the run was a full listing (null until it ran).
    async fn full_sync(&self) -> Option<bool> {
        self.0.full_sync
    }

    /// Repositories the code host returned.
    async fn repos_synced(&self) -> i32 {
        self.0.repos_synced
    }

    /// New repositories.
    async fn repos_added(&self) -> i32 {
        self.0.repos_added
    }

    /// Repositories whose metadata changed.
    async fn repos_modified(&self) -> i32 {
        self.0.repos_modified
    }

    /// Unchanged repositories.
    async fn repos_unmodified(&self) -> i32 {
        self.0.repos_unmodified
    }

    /// Repositories removed because the code host no longer lists them.
    async fn repos_deleted(&self) -> i32 {
        self.0.repos_deleted
    }

    /// Errors the code host returned while listing repositories.
    async fn repo_sync_errors(&self) -> i32 {
        self.0.repo_sync_errors
    }
}

/// A page of sync runs.
#[derive(SimpleObject)]
pub struct ExternalServiceSyncJobConnection {
    pub nodes: Vec<SyncJobNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// The result of `validateExternalServiceConfig`.
#[derive(SimpleObject)]
pub struct ExternalServiceConfigValidation {
    /// Problems that would reject a save (syntax and schema errors, semantic rules).
    pub errors: Vec<String>,
    /// Findings that do not prevent a save.
    pub warnings: Vec<String>,
}

/// The result of `checkExternalServiceConfig`.
#[derive(SimpleObject)]
pub struct ExternalServiceConfigCheck {
    /// Whether the code host answered and accepted the credentials.
    pub ok: bool,
    /// Why the check failed (invalid configuration, unreachable host, rejected token).
    pub problem: Option<String>,
    /// The code host account the credentials act as, when the host says.
    pub account: Option<String>,
    /// The token's scopes, when the code host reports them.
    pub scopes: Option<Vec<String>>,
    /// Problems that do not prevent syncing (e.g. a missing scope).
    pub warnings: Vec<String>,
}

impl ExternalServiceConfigCheck {
    fn failed(problem: String) -> Self {
        Self {
            ok: false,
            problem: Some(problem),
            account: None,
            scopes: None,
            warnings: Vec::new(),
        }
    }
}

/// A page of code host connections.
#[derive(SimpleObject)]
pub struct ExternalServiceConnection {
    pub nodes: Vec<ExternalServiceNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// Code host connection queries.
#[derive(Default)]
pub struct ExternalServicesQuery;

#[Object]
impl ExternalServicesQuery {
    /// Lists code host connections (site admins only).
    async fn external_services(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<ExternalServiceConnection> {
        require_site_admin(ctx)?;
        let store = services(ctx)?.db.external_services(None);
        let limit = page_size(first, 50, 500);
        let offset = decode_cursor(after.as_deref())?;
        let list = store
            .list(None, limit + 1, offset)
            .await
            .map_err(db_error)?;
        let total = store.count(None).await.map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, list.len());
        Ok(ExternalServiceConnection {
            nodes: list
                .into_iter()
                .take(usize::try_from(limit).unwrap_or(0))
                .map(ExternalServiceNode::new)
                .collect(),
            total_count: to_int(total),
            page_info,
        })
    }

    /// Validates a configuration against `kind`'s JSON Schema and rules without saving it,
    /// for the editor (site admins only). `REDACTED` placeholders are accepted.
    async fn validate_external_service_config(
        &self,
        ctx: &Context<'_>,
        kind: ExternalServiceKindGql,
        config: String,
    ) -> Result<ExternalServiceConfigValidation> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        Ok(match svc.registry.validate(kind.into(), &config) {
            Ok(c) => ExternalServiceConfigValidation {
                errors: Vec::new(),
                warnings: c.warnings.iter().map(ToString::to_string).collect(),
            },
            Err(e) => ExternalServiceConfigValidation {
                errors: messages(&e),
                warnings: Vec::new(),
            },
        })
    }

    /// Checks a configuration against the code host without saving it (site admins only):
    /// whether the host is reachable and accepts the token, who the token acts as and its
    /// scopes. With `id`, `REDACTED` placeholders are filled in from that connection.
    async fn check_external_service_config(
        &self,
        ctx: &Context<'_>,
        kind: ExternalServiceKindGql,
        config: String,
        id: Option<ID>,
    ) -> Result<ExternalServiceConfigCheck> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let kind: ExternalServiceKind = kind.into();
        let mut text = config;
        if let Some(id) = id {
            let id = parse_external_service_id(&id)?;
            let store = svc.db.external_services(svc.keyring.clone());
            let current = store
                .get(id)
                .await
                .map_err(db_error)?
                .ok_or_else(|| error(codes::NOT_FOUND, "External service not found."))?;
            if current.kind != kind {
                return Err(error(
                    codes::INVALID_INPUT,
                    "The connection is of a different kind.",
                ));
            }
            let stored = store.config(id).await.map_err(db_error)?;
            text = match svc.registry.unredact(kind, &text, &stored) {
                Ok(t) => t,
                Err(e) => return Ok(ExternalServiceConfigCheck::failed(invalid(&e))),
            };
        }
        if let Err(e) = svc.registry.validate(kind, &text) {
            return Ok(ExternalServiceConfigCheck::failed(invalid(&e)));
        }
        Ok(match check_report(svc, kind, &text).await {
            Ok(report) => ExternalServiceConfigCheck {
                ok: true,
                problem: None,
                account: report.account,
                scopes: report.scopes,
                warnings: report.warnings,
            },
            Err(problem) => ExternalServiceConfigCheck::failed(problem),
        })
    }
}

/// Input of `addExternalService`.
#[derive(InputObject)]
pub struct AddExternalServiceInput {
    /// The code host kind.
    pub kind: ExternalServiceKindGql,
    /// The name shown in the UI.
    pub display_name: String,
    /// The JSONC configuration.
    pub config: String,
}

/// Input of `updateExternalService`.
#[derive(InputObject)]
pub struct UpdateExternalServiceInput {
    /// The connection to update.
    pub id: ID,
    /// A new display name.
    pub display_name: Option<String>,
    /// A new configuration; `REDACTED` placeholders keep the stored secrets.
    pub config: Option<String>,
}

fn messages(e: &ConfigError) -> Vec<String> {
    match e {
        ConfigError::Syntax(s) => vec![format!("syntax error: {s}")],
        ConfigError::Invalid(issues) => issues.iter().map(ToString::to_string).collect(),
    }
}

/// `e` as one line of text for a check result.
fn invalid(e: &ConfigError) -> String {
    format!("The configuration is invalid: {}", messages(e).join("; "))
}

fn validation_error(e: &ConfigError) -> async_graphql::Error {
    let messages = messages(e);
    let message = format!(
        "The configuration is invalid:\n{}",
        messages
            .iter()
            .map(|m| format!("- {m}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    async_graphql::ErrorExtensions::extend_with(async_graphql::Error::new(message), |_, ext| {
        ext.set("code", codes::VALIDATION);
        ext.set("messages", messages.clone());
    })
}

/// Validates `config`; returns the warnings as text.
fn validate(svc: &Services, kind: ExternalServiceKind, config: &str) -> Result<Vec<String>> {
    svc.registry
        .validate(kind, config)
        .map(|c| c.warnings.iter().map(ToString::to_string).collect())
        .map_err(|e| validation_error(&e))
}

/// Checks the connection against the code host; problems become warnings.
async fn check(svc: &Services, kind: ExternalServiceKind, config: &str) -> Vec<String> {
    match check_report(svc, kind, config).await {
        Ok(report) => report.warnings,
        Err(problem) => vec![problem],
    }
}

/// Checks the connection against the code host. `Err` is the reason it failed.
async fn check_report(
    svc: &Services,
    kind: ExternalServiceKind,
    config: &str,
) -> std::result::Result<CheckReport, String> {
    let source = svc
        .registry
        .source(kind, config)
        .map_err(|e| format!("could not check the connection: {e}"))?;
    match tokio::time::timeout(CHECK_TIMEOUT, source.check()).await {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(e)) => Err(format!("could not reach the code host: {e}")),
        Err(_) => Err("could not reach the code host: timed out".to_owned()),
    }
}

fn warning(mut warnings: Vec<String>) -> Option<String> {
    warnings.retain(|w| !w.is_empty());
    (!warnings.is_empty()).then(|| warnings.join("\n"))
}

fn require_keyring(svc: &Services) -> Result<()> {
    if svc.keyring.is_none() {
        return Err(error(
            codes::INTERNAL,
            "Encryption keys are not configured (SOURCEGULL_KEYS_FILE or SOURCEGULL_KEYS); code host connections \
             cannot be stored.",
        ));
    }
    Ok(())
}

/// Code host connection mutations.
#[derive(Default)]
pub struct ExternalServicesMutation;

#[Object]
impl ExternalServicesMutation {
    /// Adds a code host connection and queues its first sync (site admins only).
    async fn add_external_service(
        &self,
        ctx: &Context<'_>,
        input: AddExternalServiceInput,
    ) -> Result<ExternalServiceNode> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        require_keyring(svc)?;
        declared::ensure_creatable(svc, declared::kinds::CONNECTIONS).await?;
        let kind: ExternalServiceKind = input.kind.into();
        let display_name = input.display_name.trim().to_owned();
        if display_name.is_empty() {
            return Err(error(
                codes::INVALID_INPUT,
                "The display name must not be empty.",
            ));
        }
        let mut warnings = validate(svc, kind, &input.config)?;
        let created = svc
            .db
            .external_services(svc.keyring.clone())
            .create(&NewExternalService {
                kind,
                display_name,
                config: input.config.clone(),
            })
            .await
            .map_err(db_error)?;
        svc.db
            .external_services(svc.keyring.clone())
            .set_unrestricted(created.id, svc.registry.unrestricted(kind, &input.config))
            .await
            .map_err(db_error)?;
        warnings.extend(check(svc, kind, &input.config).await);
        audit::record(
            svc,
            events::EXTERNAL_SERVICE_CREATED,
            Some(actor.user_id),
            json!({ "id": created.id, "kind": kind.as_str() }),
        )
        .await;
        Ok(ExternalServiceNode {
            service: created,
            warning: warning(warnings),
        })
    }

    /// Updates a code host connection (site admins only). A configuration change queues a
    /// full sync.
    async fn update_external_service(
        &self,
        ctx: &Context<'_>,
        input: UpdateExternalServiceInput,
    ) -> Result<ExternalServiceNode> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        require_keyring(svc)?;
        let id = parse_external_service_id(&input.id)?;
        declared::ensure_not_declared(svc, declared::kinds::CONNECTIONS, &id.0.to_string()).await?;
        let store = svc.db.external_services(svc.keyring.clone());
        let current = store
            .get(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "External service not found."))?;
        let display_name = match input.display_name.map(|n| n.trim().to_owned()) {
            Some(n) if n.is_empty() => {
                return Err(error(
                    codes::INVALID_INPUT,
                    "The display name must not be empty.",
                ));
            }
            other => other,
        };
        let mut warnings = Vec::new();
        let (updated, changed_config) = match input.config {
            None => {
                let update = ExternalServiceUpdate {
                    display_name,
                    config: None,
                    config_keeps_sync: false,
                };
                (store.update(id, &update).await.map_err(db_error)?, None)
            }
            Some(new) => {
                // The row stays locked from reading the stored config to writing the merged
                // one, so secrets rotated meanwhile (OAuth token refresh) are not overwritten
                // with the values they replaced.
                let locked = store.lock_config(id).await.map_err(db_error)?;
                let merged = svc
                    .registry
                    .unredact(current.kind, &new, &locked.config)
                    .map_err(|e| validation_error(&e))?;
                warnings = validate(svc, current.kind, &merged)?;
                // Unchanged config: no re-encryption and no forced full sync.
                let config = (merged != locked.config).then_some(merged);
                let update = ExternalServiceUpdate {
                    display_name,
                    config: config.clone(),
                    config_keeps_sync: false,
                };
                (locked.update(&update).await.map_err(db_error)?, config)
            }
        };
        if let Some(config) = &changed_config {
            store
                .set_unrestricted(id, svc.registry.unrestricted(current.kind, config))
                .await
                .map_err(db_error)?;
            warnings.extend(check(svc, current.kind, config).await);
        }
        audit::record(
            svc,
            events::EXTERNAL_SERVICE_UPDATED,
            Some(actor.user_id),
            json!({ "id": id, "configChanged": changed_config.is_some() }),
        )
        .await;
        Ok(ExternalServiceNode {
            service: updated,
            warning: warning(warnings),
        })
    }

    /// Deletes a code host connection; repos no other connection mirrors are removed (site
    /// admins only). `async` is accepted for compatibility; deletion is always immediate.
    async fn delete_external_service(
        &self,
        ctx: &Context<'_>,
        external_service: ID,
        #[graphql(name = "async")] _async: Option<bool>,
    ) -> Result<EmptyResponse> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_external_service_id(&external_service)?;
        declared::ensure_not_declared(svc, declared::kinds::CONNECTIONS, &id.0.to_string()).await?;
        let deleted = svc
            .db
            .external_services(None)
            .soft_delete(id)
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::EXTERNAL_SERVICE_DELETED,
            Some(actor.user_id),
            json!({ "id": id, "reposDeleted": deleted }),
        )
        .await;
        Ok(EmptyResponse)
    }

    /// Queues a sync of a code host connection now (site admins only). No-op when one is
    /// already queued.
    async fn sync_external_service(&self, ctx: &Context<'_>, id: ID) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let id = parse_external_service_id(&id)?;
        if svc
            .db
            .external_services(None)
            .get(id)
            .await
            .map_err(db_error)?
            .is_none()
        {
            return Err(error(codes::NOT_FOUND, "External service not found."));
        }
        svc.db
            .sync_jobs()
            .enqueue(id, SyncReason::Manual)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Cancels a queued or running sync job (site admins only). A running sync stops at its
    /// worker's next heartbeat.
    async fn cancel_external_service_sync(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let job = parse_sync_job_id(&id)?;
        let store = svc.db.sync_jobs();
        if store.get(job).await.map_err(db_error)?.is_none() {
            return Err(error(codes::NOT_FOUND, "Sync job not found."));
        }
        if !store.cancel(job).await.map_err(db_error)? {
            return Err(error(
                codes::INVALID_INPUT,
                "Only queued or processing sync jobs can be canceled.",
            ));
        }
        Ok(EmptyResponse)
    }

    /// Replaces the connection's webhook secrets with a new random secret and returns it with
    /// the connection (site admins only). This is the only time the secret is returned;
    /// configure it on the code host's webhook. Webhooks sent with the old secret are rejected
    /// from now on.
    /// A connection that registers its webhooks (`webhooks.autoRegister`) gets a full sync,
    /// which updates the registered hooks to the new secret.
    async fn regenerate_external_service_webhook_secret(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> Result<RegenerateWebhookSecretPayload> {
        let actor = require_site_admin(ctx)?;
        let svc = services(ctx)?;
        require_keyring(svc)?;
        let id = parse_external_service_id(&id)?;
        declared::ensure_not_declared(svc, declared::kinds::CONNECTIONS, &id.0.to_string()).await?;
        let store = svc.db.external_services(svc.keyring.clone());
        let current = store
            .get(id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "External service not found."))?;
        // Locked from read to write, so a concurrent token refresh is kept.
        let locked = store.lock_config(id).await.map_err(db_error)?;
        // 32 random bytes, URL-safe: valid as a GitLab, GitHub or Gitea webhook secret.
        let secret = sourcegull_crypto::session::generate_session_id();
        let config = svc
            .registry
            .set_webhook_secret(current.kind, &locked.config, &secret)
            .map_err(|e| validation_error(&e))?;
        validate(svc, current.kind, &config)?;
        let keeps_sync = !svc.registry.registers_webhooks(current.kind, &config);
        let updated = locked
            .update(&ExternalServiceUpdate {
                display_name: None,
                config: Some(config),
                config_keeps_sync: keeps_sync,
            })
            .await
            .map_err(db_error)?;
        audit::record(
            svc,
            events::EXTERNAL_SERVICE_UPDATED,
            Some(actor.user_id),
            json!({ "id": id, "webhookSecretRegenerated": true }),
        )
        .await;
        Ok(RegenerateWebhookSecretPayload {
            external_service: ExternalServiceNode::new(updated),
            webhook_secret: secret,
        })
    }
}

/// The result of `regenerateExternalServiceWebhookSecret`.
#[derive(SimpleObject)]
#[graphql(name = "RegenerateExternalServiceWebhookSecretPayload")]
pub struct RegenerateWebhookSecretPayload {
    /// The connection, with `webhookSecret` masked.
    external_service: ExternalServiceNode,
    /// The new webhook secret. It is not returned again.
    webhook_secret: String,
}

/// Secrets shorter than this are masked as just `…`.
const MASK_MIN_LEN: usize = 12;
/// How many trailing characters a masked secret keeps.
const MASK_TAIL: usize = 4;

/// The last characters of `secret` after `…`, or just `…` when it is short.
fn mask_tail(secret: &str) -> String {
    let n = secret.chars().count();
    if n < MASK_MIN_LEN {
        return "…".to_owned();
    }
    let tail: String = secret.chars().skip(n - MASK_TAIL).collect();
    format!("…{tail}")
}

/// Loads a live connection for `node(id:)`.
pub async fn load_node(svc: &Services, id: i64) -> Result<Option<ExternalServiceNode>> {
    Ok(svc
        .db
        .external_services(None)
        .get(ExternalServiceId(id))
        .await
        .map_err(db_error)?
        .map(ExternalServiceNode::new))
}
