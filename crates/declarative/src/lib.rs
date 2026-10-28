//! Configuration as code. The leader worker applies two files to the database and keeps them
//! applied:
//!
//! - the resources file (`SOURCEGULL_RESOURCES_FILE`, [`sourcegull_config::resources`]): connections,
//!   search contexts, roles, site admins, outbound webhooks, executor secrets, code
//!   intelligence policies and feature flags ([`apply`]);
//! - the global settings file (`SOURCEGULL_GLOBAL_SETTINGS_FILE`): the whole global settings
//!   document, written as a new settings version when it changes.
//!
//! [`Reconciler::run`] re-reads both every [`POLL_INTERVAL`] (secret references included, so
//! a rotated secret is picked up) and applies what changed. A file that cannot be read or
//! checked changes nothing: the last applied state stays and the error is recorded in
//! `declared_sources` (shown to site admins) and in [`ERRORS_METRIC`]. A file that is no longer
//! configured releases what it managed to the UI. The API keeps what a file owns read-only
//! (`sourcegull_db::DeclaredStore::ownership`).

pub mod apply;
pub mod check;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use sourcegull_config::refs::{ProcessRefs, RefSource};
use sourcegull_config::{SecurityEventLogLocation, SiteConfigHandle};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, GLOBAL_SETTINGS_SOURCE, NewSecurityEvent, RESOURCES_SOURCE};
use sourcegull_repos::Registry;
use sourcegull_types::SettingsSubject;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

pub use apply::{Applier, ApplyError, ApplyReport};
pub use check::{CheckedResources, check_global_settings, check_resources, describe_issues};

/// Lease name of the reconciler.
pub const RECONCILER: &str = "declared-config";
/// How often the files are re-read.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Gauge per source (`resources`, `global_settings`): 1 while its file cannot be applied.
pub const ERRORS_METRIC: &str = metric_names::DECLARED_CONFIG_ERRORS;
/// Counter of changes made by the files, by `kind` and `action`.
pub const CHANGES_METRIC: &str = metric_names::DECLARED_CONFIG_CHANGES_TOTAL;

/// Advisory lock held while applying, so two workers never apply at once (a lease
/// handover can briefly overlap).
const APPLY_LOCK: i64 = 0x5345_454b_4552_0003; // "SOURCEGULL" + 3

/// Security events recorded for changes made by the files.
pub mod events {
    /// A declared object was created, updated, taken over, deleted or released.
    pub const DECLARED_RESOURCE_CHANGED: &str = "DeclaredResourceChanged";
    /// The file granted or took back site admin or a role.
    pub const DECLARED_GRANT_CHANGED: &str = "DeclaredGrantChanged";
    /// The global settings file was written as a new global settings version.
    pub const DECLARED_GLOBAL_SETTINGS_APPLIED: &str = "DeclaredGlobalSettingsApplied";
}

/// The files to apply; `None` when not configured.
#[derive(Debug, Clone, Default)]
pub struct Files {
    pub resources: Option<PathBuf>,
    pub global_settings: Option<PathBuf>,
}

/// The outcome of one pass over both files.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    /// What applying the resources file changed (`None`: not configured or not applied).
    pub resources: Option<ApplyReport>,
    /// Why the resources file was not applied (or which entries failed).
    pub resources_error: Option<String>,
    /// Whether a new global settings version was written.
    pub global_settings_written: bool,
    pub global_settings_error: Option<String>,
}

/// Applies the configured files.
#[derive(Clone)]
pub struct Reconciler {
    db: Db,
    applier: Applier,
    files: Files,
    config: Option<SiteConfigHandle>,
    refs: Arc<dyn RefSource + Send + Sync>,
}

impl std::fmt::Debug for Reconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reconciler")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl Reconciler {
    /// A reconciler for `files`. `config` decides where security events go (the database when
    /// `None`).
    pub fn new(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        files: Files,
        config: Option<SiteConfigHandle>,
    ) -> Self {
        Self {
            applier: Applier::new(db.clone(), keyring, Registry::default()),
            db,
            files,
            config,
            refs: Arc::new(ProcessRefs),
        }
    }

    /// Looks secret references up in `refs` instead of the process environment (tests).
    pub fn with_refs(mut self, refs: Arc<dyn RefSource + Send + Sync>) -> Self {
        self.refs = refs;
        self
    }

    /// Applies both files every [`POLL_INTERVAL`] until `cancel` fires.
    pub async fn run(self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_errors: (Option<String>, Option<String>) = (None, None);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            let report = self.apply_once().await;
            let errors = (
                report.resources_error.clone(),
                report.global_settings_error.clone(),
            );
            if errors != last_errors {
                if let Some(e) = &errors.0 {
                    tracing::error!(error = %e, "the resources file could not be fully applied");
                }
                if let Some(e) = &errors.1 {
                    tracing::error!(error = %e, "the global settings file could not be applied");
                }
                last_errors = errors;
            }
        }
    }

    /// One pass over both files.
    pub async fn apply_once(&self) -> PassReport {
        let mut report = PassReport::default();
        // A transaction-level lock: if this pass is dropped midway, the transaction rolls
        // back when its connection returns to the pool, and the lock goes with it.
        let mut lock = match self.db.pool().begin().await {
            Ok(c) => c,
            Err(e) => {
                report.resources_error = Some(format!("database error: {e}"));
                return report;
            }
        };
        if let Err(e) = sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(APPLY_LOCK)
            .execute(&mut *lock)
            .await
        {
            report.resources_error = Some(format!("database error: {e}"));
            return report;
        }
        match self.apply_resources().await {
            Ok(r) => {
                if let Some(r) = &r
                    && !r.errors.is_empty()
                {
                    report.resources_error = Some(r.errors.join("\n"));
                }
                report.resources = r;
            }
            Err(e) => report.resources_error = Some(e),
        }
        match self.apply_global_settings().await {
            Ok(written) => report.global_settings_written = written,
            Err(e) => report.global_settings_error = Some(e),
        }
        if let Err(e) = lock.rollback().await {
            tracing::warn!(error = %e, "releasing the apply lock failed");
        }
        metrics::gauge!(ERRORS_METRIC, "source" => RESOURCES_SOURCE)
            .set(f64::from(u8::from(report.resources_error.is_some())));
        metrics::gauge!(ERRORS_METRIC, "source" => GLOBAL_SETTINGS_SOURCE)
            .set(f64::from(u8::from(report.global_settings_error.is_some())));
        report
    }

    async fn apply_resources(&self) -> Result<Option<ApplyReport>, String> {
        let declared = self.db.declared();
        let Some(path) = &self.files.resources else {
            if declared
                .source(RESOURCES_SOURCE)
                .await
                .map_err(db)?
                .is_some()
            {
                let report = self
                    .applier
                    .release_all()
                    .await
                    .map_err(|e| e.to_string())?;
                declared.remove_source(RESOURCES_SOURCE).await.map_err(db)?;
                tracing::info!(
                    "SOURCEGULL_RESOURCES_FILE is no longer set; its objects are editable in the UI again"
                );
                self.record_changes(&report).await;
                return Ok(Some(report));
            }
            return Ok(None);
        };
        let shown = path.display().to_string();
        let checked = match read(path).and_then(|text| {
            let config = self
                .config
                .as_ref()
                .map(|c| c.load().value.clone())
                .unwrap_or_default();
            check_resources(&text, self.refs.as_ref(), &self.applier.registry, &config)
                .map_err(|issues| describe_issues(&issues))
        }) {
            Ok(c) => c,
            Err(e) => {
                declared
                    .record_error(RESOURCES_SOURCE, &shown, &e)
                    .await
                    .map_err(db)?;
                return Err(e);
            }
        };
        let resources = &checked.resources;
        let report = match self.applier.apply(resources).await {
            Ok(mut r) => {
                r.warnings
                    .extend(checked.warnings.iter().map(ToString::to_string));
                r
            }
            Err(e) => {
                let e = resources.resolved.redact(&e.to_string());
                declared
                    .record_error(RESOURCES_SOURCE, &shown, &e)
                    .await
                    .map_err(db)?;
                return Err(e);
            }
        };
        let kinds: Vec<String> = resources
            .kinds()
            .iter()
            .map(|k| k.as_str().to_owned())
            .collect();
        if report.errors.is_empty() {
            declared
                .record_applied(
                    RESOURCES_SOURCE,
                    &shown,
                    &kinds,
                    resources.exclusive,
                    &resources.hash,
                )
                .await
                .map_err(db)?;
        } else {
            // Applied entries stay applied; the file stays in error until every entry is.
            let errors: Vec<String> = report
                .errors
                .iter()
                .map(|e| resources.resolved.redact(e))
                .collect();
            declared
                .record_applied(
                    RESOURCES_SOURCE,
                    &shown,
                    &kinds,
                    resources.exclusive,
                    &resources.hash,
                )
                .await
                .map_err(db)?;
            declared
                .record_error(RESOURCES_SOURCE, &shown, &errors.join("\n"))
                .await
                .map_err(db)?;
        }
        for w in &report.warnings {
            tracing::debug!(warning = %w, "resources file");
        }
        self.record_changes(&report).await;
        Ok(Some(report))
    }

    async fn apply_global_settings(&self) -> Result<bool, String> {
        let declared = self.db.declared();
        let Some(path) = &self.files.global_settings else {
            if declared
                .remove_source(GLOBAL_SETTINGS_SOURCE)
                .await
                .map_err(db)?
            {
                tracing::info!(
                    "SOURCEGULL_GLOBAL_SETTINGS_FILE is no longer set; global settings are editable in the UI again"
                );
            }
            return Ok(false);
        };
        let shown = path.display().to_string();
        let text = match read(path).and_then(|text| {
            check_global_settings(&text)
                .map(|()| text)
                .map_err(|issues| describe_issues(&issues))
        }) {
            Ok(t) => t,
            Err(e) => {
                declared
                    .record_error(GLOBAL_SETTINGS_SOURCE, &shown, &e)
                    .await
                    .map_err(db)?;
                return Err(e);
            }
        };
        let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        let settings = self.db.settings();
        let latest = settings.latest(SettingsSubject::Global).await.map_err(db)?;
        let written = match &latest {
            Some(l) if l.contents == text => false,
            _ => {
                settings
                    .create(
                        SettingsSubject::Global,
                        &text,
                        None,
                        latest.as_ref().map(|l| l.id),
                    )
                    .await
                    .map_err(db)?;
                true
            }
        };
        declared
            .record_applied(GLOBAL_SETTINGS_SOURCE, &shown, &[], false, &hash)
            .await
            .map_err(db)?;
        if written {
            tracing::info!(path = %shown, "global settings written from file");
            self.audit(
                events::DECLARED_GLOBAL_SETTINGS_APPLIED,
                serde_json::json!({ "path": shown }),
            )
            .await;
        }
        Ok(written)
    }

    async fn record_changes(&self, report: &ApplyReport) {
        for (kind, key, action) in &report.changes {
            tracing::info!(kind, key, action, "resources file applied");
            metrics::counter!(CHANGES_METRIC, "kind" => kind.clone(), "action" => *action)
                .increment(1);
            self.audit(
                events::DECLARED_RESOURCE_CHANGED,
                serde_json::json!({ "kind": kind, "key": key, "action": action }),
            )
            .await;
        }
        for (kind, key, user, action) in &report.grants {
            tracing::info!(kind, key, user = user.0, action, "resources file applied");
            self.audit(
                events::DECLARED_GRANT_CHANGED,
                serde_json::json!({ "kind": kind, "key": key, "userID": user.0, "action": action }),
            )
            .await;
        }
    }

    async fn audit(&self, name: &str, argument: serde_json::Value) {
        let location = self
            .config
            .as_ref()
            .map(|c| c.load().value.log.security_event_log.location)
            .unwrap_or_default();
        if matches!(
            location,
            SecurityEventLogLocation::Logs | SecurityEventLogLocation::All
        ) {
            tracing::info!(target: "security_event", event = name, argument = %argument);
        }
        if matches!(
            location,
            SecurityEventLogLocation::Database | SecurityEventLogLocation::All
        ) {
            let event = NewSecurityEvent {
                name: name.to_owned(),
                source: "BACKEND".to_owned(),
                argument,
                version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            };
            if let Err(e) = self.db.security_events().insert(&event).await {
                tracing::warn!(error = %e, "recording a security event failed");
            }
        }
    }
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))
}

fn db(e: sourcegull_db::DbError) -> String {
    format!("database error: {e}")
}
