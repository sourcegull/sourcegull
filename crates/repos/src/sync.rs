//! Discovery sync of one code host connection.
//!
//! A sync lists the connection's repos through its [`RepoSource`] and upserts them in batches
//! (new repos get a clone request; repos whose `last_activity_at` moved get a fetch request).
//! It is a full listing when the connection has never completed one, when the last one is
//! older than [`FULL_SYNC_INTERVAL`], or when the source cannot list incrementally; otherwise
//! it only asks for repos active since the watermark. Only a full listing without errors
//! deletes: repos the connection no longer lists are unlinked, and soft-deleted when no other
//! connection lists them.
//!
//! A connection that registers its webhooks (`webhooks.autoRegister`) then has a hook ensured
//! on every listed repo after a full listing, and on new repos after an incremental one, and
//! the hooks removed from repos it no longer lists. The hook URL is the connection's webhook
//! URL with a fingerprint of the secret in its query (`?v=`): code hosts do not return a hook's
//! secret, so a changed secret shows up as a changed URL and the hook is updated. Hook problems
//! (e.g. a token without the Maintainer role on a project) are recorded as warnings on the
//! sync job and never fail the sync.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use sourcegull_codehost::{CodeHostError, HookChange, HookTarget, ListOptions, RepoMeta};
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{
    Db, DbError, ListedRepo, PermsSyncReason, PermsSyncSubject, SyncOutcome, SyncStats,
};
use sourcegull_types::{ExternalService, ExternalServiceId, RepoId};
use tokio_util::sync::CancellationToken;

use crate::Registry;

/// How often a connection gets a full (non-incremental) listing.
pub const FULL_SYNC_INTERVAL: Duration = Duration::from_secs(3600);
/// Repos upserted per transaction.
pub const BATCH_SIZE: usize = 100;
/// Listing errors kept in the sync's error message.
const MAX_REPORTED_ERRORS: usize = 5;
/// Warnings kept on a sync job.
const MAX_WARNINGS: usize = 20;

/// Why a sync failed.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The connection's configuration is unusable (or cannot be decrypted).
    #[error("{0}")]
    Config(String),
    /// The listing was incomplete; what was listed is saved, nothing was deleted.
    #[error("{message}")]
    Partial {
        /// Counters of what was done.
        stats: SyncStats,
        /// The listing errors.
        message: String,
    },
    /// The sync was cancelled (shutdown or lost lease).
    #[error("cancelled")]
    Cancelled,
    /// Database error.
    #[error(transparent)]
    Db(#[from] DbError),
}

/// Runs discovery syncs.
#[derive(Debug, Clone)]
pub struct Syncer {
    db: Db,
    keyring: Option<Arc<Keyring>>,
    registry: Registry,
    config: SiteConfigHandle,
}

impl Syncer {
    /// A syncer reading connection configs with `keyring` and the sync interval from `config`.
    pub fn new(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        registry: Registry,
        config: SiteConfigHandle,
    ) -> Self {
        Self {
            db,
            keyring,
            registry,
            config,
        }
    }

    /// The delay until a connection's next scheduled sync (`repoListUpdateInterval`).
    pub fn interval(&self) -> Duration {
        let minutes = self.config.load().value.repo_list_update_interval.max(1);
        Duration::from_secs(u64::from(minutes) * 60)
    }

    /// Whether the next sync of `svc` must be a full listing.
    pub fn needs_full(svc: &ExternalService, now: DateTime<Utc>) -> bool {
        match (svc.sync_watermark, svc.last_full_sync_at) {
            (Some(_), Some(last_full)) => {
                now.signed_duration_since(last_full)
                    .to_std()
                    .unwrap_or_default()
                    >= FULL_SYNC_INTERVAL
            }
            _ => true,
        }
    }

    /// Queues repo-centric permissions syncs of new repositories. Failures are logged:
    /// the permissions scheduler picks never-synced repositories first anyway.
    async fn queue_perms_syncs(&self, added: &[RepoId]) {
        let jobs = self.db.perms_sync_jobs();
        for &repo in added {
            if let Err(e) = jobs
                .enqueue(
                    PermsSyncSubject::Repo(repo),
                    PermsSyncReason::RepoCreated,
                    None,
                )
                .await
            {
                tracing::warn!(repo = repo.get(), error = %e, "queueing a permissions sync failed");
            }
        }
    }

    /// Syncs connection `id` and records the outcome on the connection (and on sync job `job`
    /// when given). A deleted connection is a no-op.
    pub async fn sync(
        &self,
        id: ExternalServiceId,
        job: Option<i64>,
        cancel: &CancellationToken,
    ) -> Result<SyncStats, SyncError> {
        let store = self.db.external_services(self.keyring.clone());
        let Some(svc) = store.get(id).await? else {
            return Ok(SyncStats::default());
        };
        let next_sync_at =
            Utc::now() + chrono::Duration::from_std(self.interval()).unwrap_or_default();
        let result = self.run(&svc, cancel).await;
        let (stats, watermark, error) = match &result {
            Ok((stats, watermark, warnings)) => {
                if let Some(job) = job
                    && !warnings.is_empty()
                {
                    self.db.sync_jobs().record_warnings(job, warnings).await?;
                }
                (Some(*stats), *watermark, None)
            }
            Err(SyncError::Partial { stats, message }) => {
                (Some(*stats), None, Some(message.clone()))
            }
            Err(SyncError::Cancelled) => return Err(SyncError::Cancelled),
            Err(e) => (None, None, Some(e.to_string())),
        };
        if let (Some(job), Some(stats)) = (job, &stats) {
            self.db.sync_jobs().record_stats(job, stats).await?;
        }
        let watermark = match (svc.sync_watermark, watermark) {
            (Some(old), Some(new)) => Some(old.max(new)),
            (old, new) => new.or(old),
        };
        store
            .record_sync(
                id,
                &SyncOutcome {
                    full: stats.is_some_and(|s| s.full),
                    watermark,
                    error,
                    next_sync_at,
                },
            )
            .await?;
        result.map(|(stats, _, _)| stats)
    }

    /// Lists and upserts; returns the counters, the newest activity seen and warnings.
    async fn run(
        &self,
        svc: &ExternalService,
        cancel: &CancellationToken,
    ) -> Result<(SyncStats, Option<DateTime<Utc>>, Vec<String>), SyncError> {
        let config = self
            .db
            .external_services(self.keyring.clone())
            .config(svc.id)
            .await
            .map_err(|e| match e {
                DbError::Invalid(m) => SyncError::Config(m.to_owned()),
                DbError::Crypto(c) => SyncError::Config(format!("decrypting the config: {c}")),
                other => SyncError::Db(other),
            })?;
        self.db
            .external_services(self.keyring.clone())
            .set_unrestricted(svc.id, self.registry.unrestricted(svc.kind, &config))
            .await
            .map_err(SyncError::Db)?;
        let source = self
            .registry
            .for_connection(svc.id.get())
            .source(svc.kind, &config)
            .map_err(|e| SyncError::Config(e.to_string()))?;
        let repo_centric = self.registry.repo_centric_perms(svc.kind, &config);
        let full = !source.supports_incremental() || Self::needs_full(svc, Utc::now());
        let opts = ListOptions {
            since: if full { None } else { svc.sync_watermark },
        };
        tracing::info!(
            external_service = svc.id.get(),
            full,
            "syncing code host connection"
        );

        let repos = self.db.repos();
        let mut stats = SyncStats {
            full,
            ..SyncStats::default()
        };
        let mut seen: Vec<RepoId> = Vec::new();
        // (external id, name) of the repos to ensure a webhook on.
        let mut hook_repos: Vec<(String, String)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut error_count = 0usize;
        let mut newest: Option<DateTime<Utc>> = None;
        let mut batch: Vec<ListedRepo> = Vec::with_capacity(BATCH_SIZE);
        let mut stream = source.list_repos(opts);
        loop {
            let item = tokio::select! {
                () = cancel.cancelled() => return Err(SyncError::Cancelled),
                item = stream.next() => item,
            };
            let done = item.is_none();
            match item {
                Some(Ok(meta)) => {
                    newest = newest.max(meta.last_activity_at);
                    batch.push(listed(meta));
                }
                Some(Err(e)) => {
                    error_count += 1;
                    if errors.len() < MAX_REPORTED_ERRORS {
                        errors.push(e.to_string());
                    }
                }
                None => {}
            }
            if batch.len() >= BATCH_SIZE || (done && !batch.is_empty()) {
                let summary = repos.upsert_listed(svc.id, &batch).await?;
                if repo_centric {
                    // Public repositories are readable by everyone: nothing to sync.
                    let private: Vec<RepoId> = summary
                        .ids
                        .iter()
                        .zip(&batch)
                        .filter(|(id, r)| r.private && summary.added.contains(id))
                        .map(|(id, _)| *id)
                        .collect();
                    self.queue_perms_syncs(&private).await;
                }
                stats.added += count(summary.added.len());
                stats.modified += count(summary.modified.len());
                stats.unmodified += count(summary.unmodified.len());
                stats.synced += count(summary.ids.len());
                hook_repos.extend(
                    summary
                        .ids
                        .iter()
                        .zip(&batch)
                        .filter(|(id, _)| full || summary.added.contains(id))
                        .map(|(_, r)| (r.external_id.clone(), r.name.clone())),
                );
                seen.extend(summary.ids);
                batch.clear();
            }
            if done {
                break;
            }
        }
        drop(stream);

        if !errors.is_empty() {
            let mut message = format!(
                "listing repositories failed ({error_count} error(s); nothing was deleted): {}",
                errors.join("; ")
            );
            if error_count > errors.len() {
                message.push_str("; …");
            }
            tracing::warn!(external_service = svc.id.get(), %message, "sync incomplete");
            stats.errors = count(error_count);
            return Err(SyncError::Partial { stats, message });
        }
        let mut unlisted: Vec<(String, String)> = Vec::new();
        if full {
            if self.registry.registers_webhooks(svc.kind, &config) {
                let kept: std::collections::HashSet<RepoId> = seen.iter().copied().collect();
                for (id, external_id) in repos.linked_external_ids(svc.id).await? {
                    if !kept.contains(&id) {
                        unlisted.push((external_id, format!("repository {}", id.get())));
                    }
                }
            }
            let deleted = repos.unlink_unlisted(svc.id, &seen).await?;
            stats.deleted = count(usize::try_from(deleted).unwrap_or(usize::MAX));
        }
        let warnings = self
            .register_webhooks(svc, &config, &hook_repos, &unlisted, cancel)
            .await?;
        tracing::info!(
            external_service = svc.id.get(),
            full,
            synced = stats.synced,
            added = stats.added,
            modified = stats.modified,
            deleted = stats.deleted,
            "sync finished"
        );
        Ok((stats, newest, warnings))
    }

    /// The hook target of connection `svc` with webhook secret `secret`: its webhook URL under
    /// `externalURL` with the secret's fingerprint. Errs with a warning when `externalURL` is
    /// unset or local (the code host could not reach it).
    fn hook_target(&self, svc: &ExternalService, secret: &str) -> Result<HookTarget, String> {
        use sha2::Digest as _;
        let base = self.config.load().value.external_url.clone();
        let Some(base) = base.filter(|b| !b.trim().is_empty()) else {
            return Err(
                "webhooks are not registered: externalURL is not set in the site \
                        configuration"
                    .to_owned(),
            );
        };
        let parsed = url::Url::parse(base.trim())
            .map_err(|e| format!("webhooks are not registered: externalURL is invalid: {e}"))?;
        if is_local(&parsed) {
            return Err(format!(
                "webhooks are not registered: externalURL ({base}) is a local address the code \
                 host cannot reach"
            ));
        }
        let fingerprint = sha2::Sha256::digest(format!("sourcegull-webhook:{secret}").as_bytes());
        Ok(HookTarget {
            url: format!(
                "{}/.api/webhooks/{}?v={}",
                base.trim().trim_end_matches('/'),
                svc.webhook_uuid,
                hex::encode(&fingerprint[..6])
            ),
            secret: secret.to_owned(),
        })
    }

    /// Ensures webhooks on `ensure` and removes them from `remove` (both `(external id,
    /// name)`) when the connection registers its webhooks. Returns the warnings.
    async fn register_webhooks(
        &self,
        svc: &ExternalService,
        config: &str,
        ensure: &[(String, String)],
        remove: &[(String, String)],
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, SyncError> {
        if ensure.is_empty() && remove.is_empty() {
            return Ok(Vec::new());
        }
        let (registrar, secret): crate::registry::SecretRegistrar = match self
            .registry
            .for_connection(svc.id.get())
            .webhook_registrar(svc.kind, config)
        {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(Vec::new()),
            Err(e) => return Ok(vec![format!("webhooks are not registered: {e}")]),
        };
        let target = match self.hook_target(svc, &secret) {
            Ok(t) => t,
            Err(warning) => {
                tracing::warn!(external_service = svc.id.get(), %warning, "webhook registration skipped");
                return Ok(vec![warning]);
            }
        };
        let mut warnings = Warnings::default();
        let (mut created, mut updated, mut removed) = (0usize, 0usize, 0usize);
        for (external_id, name) in ensure {
            if cancel.is_cancelled() {
                return Err(SyncError::Cancelled);
            }
            match registrar.ensure(external_id, &target).await {
                Ok(HookChange::Created) => created += 1,
                Ok(HookChange::Updated) => updated += 1,
                Ok(HookChange::Unchanged) => {}
                Err(e) => {
                    let stop = stops_registration(&e);
                    warnings.push(hook_warning(name, &e));
                    if stop {
                        return Ok(warnings.finish());
                    }
                }
            }
        }
        for (external_id, name) in remove {
            if cancel.is_cancelled() {
                return Err(SyncError::Cancelled);
            }
            match registrar.remove(external_id, &target).await {
                Ok(n) => removed += n,
                // Repos leave a connection when the token loses access; nothing to clean up.
                Err(CodeHostError::Forbidden(_) | CodeHostError::NotFound(_)) => {}
                Err(e) => {
                    tracing::debug!(external_service = svc.id.get(), %name, error = %e, "removing a webhook failed");
                    if stops_registration(&e) {
                        break;
                    }
                }
            }
        }
        tracing::info!(
            external_service = svc.id.get(),
            created,
            updated,
            removed,
            failed = warnings.total,
            "webhooks registered"
        );
        Ok(warnings.finish())
    }
}

/// Warnings capped at [`MAX_WARNINGS`], with a count of the rest.
#[derive(Default)]
struct Warnings {
    kept: Vec<String>,
    total: usize,
}

impl Warnings {
    fn push(&mut self, warning: String) {
        self.total += 1;
        if self.kept.len() < MAX_WARNINGS {
            self.kept.push(warning);
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.total > self.kept.len() {
            self.kept
                .push(format!("… and {} more", self.total - self.kept.len()));
        }
        self.kept
    }
}

/// Whether an error makes registering the remaining hooks pointless (the token or the code
/// host's budget, not the repo, is the problem).
fn stops_registration(e: &CodeHostError) -> bool {
    matches!(
        e,
        CodeHostError::Unauthorized | CodeHostError::RateLimited { .. } | CodeHostError::Config(_)
    )
}

/// The warning for a hook that could not be ensured on repo `name`.
fn hook_warning(name: &str, e: &CodeHostError) -> String {
    match e {
        CodeHostError::Forbidden(_) => format!(
            "webhook not registered on {name}: the token needs the api scope and the Maintainer \
             role on the project"
        ),
        other => format!("webhook not registered on {name}: {other}"),
    }
}

/// Whether `url` points at this machine (a code host elsewhere could not reach it).
fn is_local(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => true,
    }
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Converts a discovered repo to the store's input.
pub fn listed(meta: RepoMeta) -> ListedRepo {
    ListedRepo {
        name: meta.name,
        uri: meta.uri,
        url: meta.url,
        description: meta.description,
        fork: meta.fork,
        archived: meta.archived,
        private: meta.private,
        visibility: meta.visibility,
        stars: meta.stars,
        topics: meta.topics,
        default_branch: meta.default_branch,
        last_activity_at: meta.last_activity_at,
        external_id: meta.external.id,
        external_service_type: meta.external.service_type,
        external_service_id: meta.external.service_id,
        metadata: meta.metadata,
        clone_url: meta.clone_url,
    }
}
