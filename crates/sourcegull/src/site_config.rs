//! Loading the site config and keeping it current.
//!
//! The database source is reloaded on `NOTIFY site_config` (through a dedicated listener
//! connection), every [`POLL_INTERVAL`] as a fallback for lost notifications, and in full after
//! the listener reconnects. The file source (`SOURCEGULL_SITE_CONFIG_FILE`) is polled; its
//! `${env:…}` and `${file:…}` references ([`sourcegull_config::refs`]) are resolved on every poll,
//! so a changed secret file is picked up like a changed config file. In both cases
//! [`ConfigHandle::store`](sourcegull_config::ConfigHandle::store) keeps only newer versions.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use sha2::{Digest, Sha256};
use sourcegull_config::{SiteConfig, SiteConfigHandle};
use sourcegull_db::notify::channels;
use sourcegull_db::{Db, DbError};
use sourcegull_services::SiteConfigSource;
use sourcegull_types::metric_names;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Poll fallback interval.
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Gauge with the version of the loaded site config.
pub const VERSION_METRIC: &str = metric_names::SITE_CONFIG_VERSION;

/// Owns the site config [`SiteConfigHandle`] and reloads it from its source.
#[derive(Debug)]
pub struct SiteConfigLoader {
    handle: SiteConfigHandle,
    source: Source,
    external_url: Option<String>,
    /// The last version that failed validation, so it is logged once rather than every poll.
    last_rejected: Option<i64>,
}

#[derive(Debug)]
enum Source {
    Db(Db),
    File { path: PathBuf, last_hash: [u8; 32] },
}

impl SiteConfigLoader {
    /// Loads the initial config: from `file` when given (an invalid file is an error), otherwise
    /// from the database, writing the default config on first boot. `external_url` fills
    /// `externalURL` when the config leaves it unset.
    pub async fn load(
        db: &Db,
        file: Option<&Path>,
        external_url: Option<String>,
    ) -> anyhow::Result<Self> {
        let loader = match file {
            Some(path) => {
                let (validated, hash) = load_file(path)?;
                log_warnings(&validated.warnings);
                let config = with_defaults(validated.config, &external_url);
                tracing::info!(path = %path.display(), "site config loaded from file (read-only)");
                Self {
                    handle: SiteConfigHandle::new(1, config),
                    source: Source::File {
                        path: path.to_owned(),
                        last_hash: hash,
                    },
                    external_url,
                    last_rejected: None,
                }
            }
            None => {
                ensure_db_config(db).await?;
                let mut loader = Self {
                    handle: SiteConfigHandle::new(
                        0,
                        with_defaults(SiteConfig::default(), &external_url),
                    ),
                    source: Source::Db(db.clone()),
                    external_url,
                    last_rejected: None,
                };
                loader.reload().await?;
                loader
            }
        };
        loader.record_version();
        Ok(loader)
    }

    /// The handle readers use. Clones share state.
    pub fn handle(&self) -> SiteConfigHandle {
        self.handle.clone()
    }

    /// Where the config comes from.
    pub fn source(&self) -> SiteConfigSource {
        match &self.source {
            Source::Db(_) => SiteConfigSource::Database,
            Source::File { path, .. } => SiteConfigSource::File(path.clone()),
        }
    }

    /// Reloads from the source, storing the result if it is newer. Returns whether the
    /// config changed. An invalid new version is logged and skipped.
    pub async fn reload(&mut self) -> anyhow::Result<bool> {
        let (version, validated) = match &mut self.source {
            Source::Db(db) => {
                let Some(row) = db.site_config().latest().await? else {
                    return Ok(false);
                };
                if row.id.get() <= self.handle.version() {
                    return Ok(false);
                }
                (row.id.get(), validate_stored(&row.contents))
            }
            Source::File { path, last_hash } => {
                let (text, hash) = read_file(path)?;
                let resolved = resolve_file(&text);
                // A failed resolution is retried on every poll (a referenced file may appear)
                // but reported once per distinct error.
                let hash = match &resolved {
                    Ok((value, _)) => resolved_hash(value),
                    Err(e) => Sha256::new()
                        .chain_update(hash)
                        .chain_update(format!("{e:#}"))
                        .finalize()
                        .into(),
                };
                if hash == *last_hash {
                    return Ok(false);
                }
                *last_hash = hash;
                let validated = resolved
                    .and_then(|(mut value, refs)| {
                        drop_unsupported(&mut value);
                        validate_resolved(&value, &refs)
                    })
                    .with_context(|| format!("site config file {}", path.display()));
                (self.handle.version() + 1, validated)
            }
        };
        match validated {
            Ok(validated) => {
                log_warnings(&validated.warnings);
                let config = with_defaults(validated.config, &self.external_url);
                let stored = self.handle.store(version, config);
                if stored {
                    tracing::info!(version, "site config loaded");
                    self.record_version();
                }
                Ok(stored)
            }
            Err(e) => {
                if self.last_rejected != Some(version) {
                    tracing::error!(
                        version,
                        error = format!("{e:#}"),
                        "ignoring invalid site config; keeping the previous one"
                    );
                    self.last_rejected = Some(version);
                }
                Ok(false)
            }
        }
    }

    fn record_version(&self) {
        // Versions are row ids, far below f64's exact-integer range.
        metrics::gauge!(VERSION_METRIC).set(self.handle.version() as f64);
    }

    /// Keeps the config current until `cancel` fires (see the module docs).
    pub async fn run(mut self, cancel: CancellationToken) {
        let (tx, mut rx) = mpsc::channel::<Wake>(16);
        let listener_task = match &self.source {
            Source::Db(db) => Some(tokio::spawn(listen(db.clone(), tx, cancel.clone()))),
            Source::File { .. } => None,
        };
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await; // the first tick completes immediately
        loop {
            let reason = tokio::select! {
                () = cancel.cancelled() => break,
                _ = poll.tick() => "poll",
                Some(wake) = rx.recv() => match wake {
                    Wake::Notified => "notify",
                    Wake::Connected => "listener (re)connected",
                },
            };
            match self.reload().await {
                Ok(true) => tracing::debug!(reason, "site config reloaded"),
                Ok(false) => {}
                Err(e) => tracing::warn!(reason, error = %e, "reloading site config failed"),
            }
        }
        if let Some(task) = listener_task {
            task.abort();
        }
    }
}

/// Why the listener woke the reloader.
#[derive(Debug, Clone, Copy)]
enum Wake {
    /// A `site_config` notification arrived.
    Notified,
    /// The listener (re)connected; notifications may have been missed, so reload in full.
    Connected,
}

/// Subscribes to `site_config` (on the process's shared `LISTEN` connection,
/// [`sourcegull_pubsub`]) and forwards wake-ups.
async fn listen(db: Db, tx: mpsc::Sender<Wake>, cancel: CancellationToken) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(db.pool(), &[channels::SITE_CONFIG]) => s,
    };
    loop {
        let wake = tokio::select! {
            () = cancel.cancelled() => return,
            e = sub.recv() => match e {
                sourcegull_pubsub::Event::Notification { .. } => Wake::Notified,
                sourcegull_pubsub::Event::Resync => Wake::Connected,
            },
        };
        if tx.send(wake).await.is_err() {
            return;
        }
    }
}

/// Writes the default site config if the table is empty (first boot).
async fn ensure_db_config(db: &Db) -> anyhow::Result<()> {
    let store = db.site_config();
    if store.latest().await?.is_some() {
        return Ok(());
    }
    match store
        .create(sourcegull_config::default_site_config_jsonc(), None, None)
        .await
    {
        Ok(row) => {
            tracing::info!(
                version = row.id.get(),
                "wrote the default site config (first boot)"
            );
            Ok(())
        }
        // Another process wrote it first.
        Err(DbError::Conflict { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Reads, resolves and validates the site config file. The hash covers the resolved document,
/// so a changed referenced secret counts as a change.
fn load_file(path: &Path) -> anyhow::Result<(sourcegull_config::ValidatedConfig, [u8; 32])> {
    let (text, _) = read_file(path)?;
    let (mut value, refs) =
        resolve_file(&text).with_context(|| format!("site config file {}", path.display()))?;
    let hash = resolved_hash(&value);
    drop_unsupported(&mut value);
    let validated = validate_resolved(&value, &refs)
        .with_context(|| format!("site config file {}", path.display()))?;
    Ok((validated, hash))
}

/// Parses the file and resolves its secret references.
pub(crate) fn resolve_file(
    text: &str,
) -> anyhow::Result<(serde_json::Value, sourcegull_config::refs::Resolved)> {
    let mut value = sourcegull_config::parse_jsonc(text)?;
    let refs =
        sourcegull_config::refs::resolve_refs(&mut value, &sourcegull_config::refs::ProcessRefs)
            .map_err(|issues| anyhow::anyhow!(sourcegull_config::ConfigError::Invalid(issues)))?;
    Ok((value, refs))
}

fn resolved_hash(value: &serde_json::Value) -> [u8; 32] {
    Sha256::digest(value.to_string().as_bytes()).into()
}

/// Validates a config stored in the database, without its unsupported auth providers.
fn validate_stored(text: &str) -> anyhow::Result<sourcegull_config::ValidatedConfig> {
    let mut value = sourcegull_config::parse_jsonc(text)
        .map_err(|e| anyhow::anyhow!(sourcegull_config::ConfigError::from(e)))?;
    drop_unsupported(&mut value);
    sourcegull_config::validate_value(&value).map_err(|e| anyhow::anyhow!(e))
}

/// Removes the auth providers of unsupported types (`saml`) from a loaded config, logging an
/// error for each, so the rest of the config still applies. Saving such a config, and
/// `sourcegull config check`, reject it.
fn drop_unsupported(value: &mut serde_json::Value) {
    for issue in sourcegull_config::remove_unsupported_auth_providers(value) {
        tracing::error!(
            pointer = %issue.pointer,
            "site config: {}; this auth provider is ignored",
            issue.message
        );
    }
}

/// Validates a resolved document; resolved secrets are masked in the messages.
pub(crate) fn validate_resolved(
    value: &serde_json::Value,
    refs: &sourcegull_config::refs::Resolved,
) -> anyhow::Result<sourcegull_config::ValidatedConfig> {
    match sourcegull_config::validate_value(value) {
        Ok(mut v) => {
            refs.redact_issues(&mut v.warnings);
            Ok(v)
        }
        Err(sourcegull_config::ConfigError::Invalid(mut issues)) => {
            refs.redact_issues(&mut issues);
            Err(sourcegull_config::ConfigError::Invalid(issues).into())
        }
        Err(e) => Err(e.into()),
    }
}

fn read_file(path: &Path) -> anyhow::Result<(String, [u8; 32])> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading site config file {}", path.display()))?;
    let hash = Sha256::digest(text.as_bytes()).into();
    Ok((text, hash))
}

fn with_defaults(mut config: SiteConfig, external_url: &Option<String>) -> SiteConfig {
    if config.external_url.is_none() {
        config.external_url.clone_from(external_url);
    }
    config
}

fn log_warnings(warnings: &[sourcegull_config::ConfigIssue]) {
    for w in warnings {
        tracing::warn!(pointer = %w.pointer, "site config: {}", w.message);
    }
}
