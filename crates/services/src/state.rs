//! Shared application state handed to every handler.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sourcegull_authn::Authenticator;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::Db;
use sourcegull_observability::MetricsHandle;
use sourcegull_rpc::RepoClients;

/// State shared by all routes. Cheap to clone (every field is a handle).
///
/// Crates that add routes take `State<AppState>`; their routers are merged into the public
/// routes of `sourcegull-http-api`. New shared services are added here as plain fields.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The main database.
    pub db: Db,
    /// The current site config, hot-reloaded on `NOTIFY site_config`.
    pub config: SiteConfigHandle,
    /// Where the site config comes from (a file makes it read-only in the UI/API).
    pub site_config_source: SiteConfigSource,
    /// Prometheus handle, rendered on `/metrics`.
    pub metrics: MetricsHandle,
    /// Flags reported by `/readyz`.
    pub readiness: Readiness,
    /// Authentication: sessions, access tokens, sign-up/sign-in.
    pub auth: Authenticator,
    /// Request counters of `auth.rateLimits` (see [`crate::rate_limits`]), in Postgres by
    /// default so every process shares them.
    pub rate_limits: Arc<dyn sourcegull_ratelimit::KeyedLimiter>,
    /// Outbound email transport; tests can replace SMTP with an in-process recorder.
    pub email_sender: Arc<dyn crate::email::EmailSender>,
    /// Data keys for secrets at rest (code host configs, …); `None` when no key file is
    /// configured, in which case features that store secrets are unavailable.
    pub keyring: Option<Arc<Keyring>>,
    /// gRPC clients of the repo nodes, routed by repo. Without configured nodes every
    /// call fails with `ClientError::NoNode`.
    pub repo_clients: Arc<RepoClients>,
    /// The repositories search resolves `repo:` filters against.
    pub search_catalog: Arc<sourcegull_search_web::CatalogCache>,
    /// Repository permissions: every read path asks it what the viewer may read.
    pub authz: sourcegull_authz::Checker,
    /// Blob storage for SCIP uploads and code graph files; `None` disables uploads.
    pub blobs: Option<sourcegull_blobstore::Blobs>,
    /// Code graph files of SCIP uploads cached on local disk, for precise code navigation;
    /// without it navigation uses the syntactic and search-based tiers only.
    pub graphs: Option<Arc<sourcegull_codeintel_scip::GraphCache>>,
    /// Recently verified GitLab CI job tokens of SCIP uploads.
    pub ci_job_tokens: crate::ci_job_tokens::CiJobTokenCache,
    /// Applies `merge_request` and `pipeline` webhooks to changesets; without it they
    /// are only logged.
    pub changeset_hooks: Option<Arc<dyn sourcegull_repos::ChangesetHooks>>,
    /// Applies bulk change specs executed with `autoApplyEnabled` once their execution completed;
    /// without it they are left for the user to apply.
    pub auto_applier: Option<Arc<dyn crate::batches::AutoApplier>>,
}

impl AppState {
    /// Creates state with a database-backed site config and fresh (not ready) readiness flags.
    pub fn new(
        db: Db,
        config: SiteConfigHandle,
        metrics: MetricsHandle,
        auth: Authenticator,
    ) -> Self {
        let rate_limits = Arc::new(sourcegull_ratelimit::PgKeyedLimiter::new(db.pool().clone()));
        Self {
            authz: sourcegull_authz::Checker::new(db.clone(), config.clone()),
            db,
            config,
            site_config_source: SiteConfigSource::Database,
            metrics,
            readiness: Readiness::default(),
            auth,
            rate_limits,
            email_sender: Arc::new(crate::email::SmtpEmailSender),
            keyring: None,
            repo_clients: Arc::new(RepoClients::new(
                sourcegull_rpc::Router::default(),
                sourcegull_rpc::ClientConfig::default(),
            )),
            search_catalog: Arc::default(),
            blobs: None,
            graphs: None,
            ci_job_tokens: Default::default(),
            changeset_hooks: None,
            auto_applier: None,
        }
    }

    /// Sets the blob store.
    pub fn with_blobs(mut self, blobs: sourcegull_blobstore::Blobs) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Sets the code graph cache.
    pub fn with_graph_cache(mut self, graphs: Arc<sourcegull_codeintel_scip::GraphCache>) -> Self {
        self.graphs = Some(graphs);
        self
    }

    /// Sets what applies changeset webhooks.
    pub fn with_changeset_hooks(
        mut self,
        hooks: Arc<dyn sourcegull_repos::ChangesetHooks>,
    ) -> Self {
        self.changeset_hooks = Some(hooks);
        self
    }

    /// Sets what applies bulk change specs automatically.
    pub fn with_auto_applier(mut self, applier: Arc<dyn crate::batches::AutoApplier>) -> Self {
        self.auto_applier = Some(applier);
        self
    }

    /// Sets the repo node clients.
    pub fn with_repo_clients(mut self, repo_clients: Arc<RepoClients>) -> Self {
        self.repo_clients = repo_clients;
        self
    }

    /// Overrides where request counters are kept.
    pub fn with_rate_limits(mut self, limits: Arc<dyn sourcegull_ratelimit::KeyedLimiter>) -> Self {
        self.rate_limits = limits;
        self
    }

    /// Overrides outbound email delivery.
    pub fn with_email_sender(mut self, sender: Arc<dyn crate::email::EmailSender>) -> Self {
        self.email_sender = sender;
        self
    }

    /// Sends only when SMTP is configured; delivery failures are logged.
    pub async fn send_email(&self, message: crate::email::EmailMessage) {
        let config = self.config.load();
        if config.value.email_smtp.is_some()
            && let Err(e) = self.email_sender.send(&config.value, message).await
        {
            tracing::warn!(error = %e, "sending email failed");
        }
    }

    /// Sets the keyring used to encrypt and decrypt secrets at rest.
    pub fn with_keyring(mut self, keyring: Option<Arc<Keyring>>) -> Self {
        self.keyring = keyring;
        self
    }

    /// The search backend of this state (sharing its catalog cache).
    pub fn search_backend(&self) -> sourcegull_search_web::SearchBackend {
        sourcegull_search_web::SearchBackend {
            db: self.db.clone(),
            authz: self.authz.clone(),
            repo_clients: self.repo_clients.clone(),
            catalog: self.search_catalog.clone(),
            config: self.config.clone(),
            ownership: Arc::new(crate::search::StoreOwnership::new(self.db.clone())),
        }
    }

    /// Sets where the site config comes from.
    pub fn with_site_config_source(mut self, source: SiteConfigSource) -> Self {
        self.site_config_source = source;
        self
    }
}

/// The origin of the site config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteConfigSource {
    /// The `site_config` table; editable through the UI/API.
    Database,
    /// A mounted file (`SOURCEGULL_SITE_CONFIG_FILE`); read-only in the UI/API.
    File(PathBuf),
}

impl SiteConfigSource {
    /// Whether edits through the UI/API must be rejected.
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::File(_))
    }
}

/// Startup and shutdown flags reported by `/readyz`. Cheap to clone; clones share state.
///
/// Database reachability is checked live on each `/readyz` call; the other conditions are
/// set by the process as startup progresses.
#[derive(Debug, Clone, Default)]
pub struct Readiness {
    inner: Arc<ReadinessFlags>,
}

#[derive(Debug, Default)]
struct ReadinessFlags {
    migrations_current: AtomicBool,
    config_loaded: AtomicBool,
    shutting_down: AtomicBool,
}

impl Readiness {
    /// Records whether every migration embedded in this binary is applied.
    pub fn set_migrations_current(&self, current: bool) {
        self.inner
            .migrations_current
            .store(current, Ordering::Release);
    }

    /// Records whether the site config has been loaded.
    pub fn set_config_loaded(&self, loaded: bool) {
        self.inner.config_loaded.store(loaded, Ordering::Release);
    }

    /// Marks the process as draining: `/readyz` fails so load balancers stop sending traffic.
    pub fn set_shutting_down(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
    }

    /// See [`set_migrations_current`](Self::set_migrations_current).
    pub fn migrations_current(&self) -> bool {
        self.inner.migrations_current.load(Ordering::Acquire)
    }

    /// See [`set_config_loaded`](Self::set_config_loaded).
    pub fn config_loaded(&self) -> bool {
        self.inner.config_loaded.load(Ordering::Acquire)
    }

    /// See [`set_shutting_down`](Self::set_shutting_down).
    pub fn shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }
}
