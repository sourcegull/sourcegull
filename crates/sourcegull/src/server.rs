//! Running roles in-process with a shared shutdown token.

use std::sync::Arc;

use anyhow::Context as _;
use sourcegull_authn::{Authenticator, PasswordParams, PgSessionStore};
use sourcegull_crypto::RotateScope;
use sourcegull_db::Db;
use sourcegull_observability::MetricsHandle;
use sourcegull_services::AppState;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::cli::ServerArgs;
use crate::roles::{Role, RoleSet};
use crate::site_config::SiteConfigLoader;
use crate::startup::{self, SchemaState};

/// Starts `roles` and runs until `signal` fires or a component fails (which cancels the rest).
/// After `signal`, `/readyz` reports 503 for `args.shutdown_delay` while everything keeps
/// serving; then the listeners close and the roles stop.
///
/// Every process serves the internal listener (`/metrics`, `/healthz`, `/readyz`); the `web`
/// role adds the public listener and the `repo` role the repo node gRPC listener. `web`, `repo`
/// and `worker` refuse to start while migrations are pending.
pub async fn run_roles(
    args: &ServerArgs,
    roles: &RoleSet,
    db: Db,
    metrics: MetricsHandle,
    signal: CancellationToken,
) -> anyhow::Result<()> {
    // Stops the listeners and roles: `shutdown_delay` after `signal`, or at once when a
    // component fails.
    let cancel = CancellationToken::new();
    let schema_current = if roles.needs_current_schema() {
        startup::ensure_schema_current(db.pool()).await?;
        true
    } else {
        let state = startup::schema_state(db.pool()).await?;
        if state != SchemaState::Current {
            tracing::warn!(?state, "database schema is not current");
        }
        state == SchemaState::Current
    };

    let key_source = args.key_source()?;
    // The key-encryption keys unwrap the data keys persisted in `encryption_keys` (created on first
    // start). Without keys, features that store secrets (code host connections) are
    // unavailable. A new last key (a new current KEK) rotates the data keys here; the
    // re-encryption out-of-band migration then moves the secrets to the new keys.
    let keyring = match startup::load_keys(&key_source)? {
        Some(provider) if schema_current => {
            let (ring, changed) = db
                .encryption_keys()
                .load_and_rotate(Arc::new(provider), RotateScope::KekChanged)
                .await
                .context("loading the data encryption keys")?;
            if changed > 0 {
                tracing::info!(
                    current_key = ring.current_kek_id(),
                    changed,
                    "rotated the data encryption keys for a new key-encryption key; \
                     secrets are re-encrypted in the background"
                );
            }
            Some(Arc::new(ring))
        }
        Some(_) => {
            tracing::warn!("schema not current; data encryption keys not loaded");
            None
        }
        None => None,
    };
    if schema_current {
        // Register the out-of-band migrations and refuse to run a version whose deprecated
        // migrations have not finished.
        let registry = sourcegull_oobmigration::Registry::sourcegull(db.clone(), keyring.clone());
        registry
            .sync(&db)
            .await
            .context("registering out-of-band migrations")?;
        let check = sourcegull_oobmigration::check_startup(&db, &registry).await;
        match check {
            Err(e) if roles.needs_current_schema() => return Err(e),
            Err(e) => tracing::warn!(error = format!("{e:#}"), "out-of-band migrations"),
            Ok(()) => {}
        }
    }

    install_rate_limiters(args, roles, &db);

    let loader = SiteConfigLoader::load(
        &db,
        crate::cli::non_empty(&args.site_config_file).as_deref(),
        args.external_url.clone(),
    )
    .await
    .context("loading the site config")?;

    let auth = Authenticator::new(
        db.clone(),
        Arc::new(PgSessionStore::new(db.clone())),
        loader.handle(),
        PasswordParams::default(),
    );
    let config_handle = loader.handle();
    // GitLab OAuth connection tokens, refreshed and stored back into their connections.
    sourcegull_repos::oauth_tokens::set_global(Arc::new(
        sourcegull_repos::oauth_tokens::ConnectionTokens::new(
            db.external_services(keyring.clone()),
            config_handle.clone(),
        ),
    ));
    if schema_current {
        // Routine runs and the outbound request log, for the site admin pages.
        crate::admin_logs::install(&db, config_handle.clone(), cancel.clone());
    }
    let topology = sourcegull_repo_node::topology::resolve(
        &args.repo.repo_nodes,
        args.repo.repo_node_addr.as_deref(),
        args.repo.repo_grpc_addr,
        roles.contains(Role::Repo),
        hostname().as_deref(),
    )?;
    let repo_clients = crate::routing::repo_clients(&topology.nodes);
    if roles.contains(Role::Web) && topology.nodes.is_empty() {
        tracing::warn!("SOURCEGULL_REPO_NODES is empty: repository contents are unavailable");
    }
    let blobs = sourcegull_blobstore::open(&args.blob_store_url())
        .with_context(|| format!("opening the blob store {}", args.blob_store_url()))?;
    blobs
        .init()
        .await
        .with_context(|| format!("preparing the blob store {}", args.blob_store_url()))?;
    let mut state = AppState::new(db.clone(), config_handle.clone(), metrics, auth)
        .with_blobs(blobs.clone())
        .with_site_config_source(loader.source())
        .with_keyring(keyring.clone())
        .with_repo_clients(repo_clients.clone())
        .with_changeset_hooks(Arc::new(
            sourcegull_batches_reconciler::ChangesetWebhooks::new(db.clone()),
        ));
    if roles.contains(Role::Web) {
        let dir = args.data_dir.join("cache").join("codegraph");
        let graphs = sourcegull_codeintel_scip::GraphCache::new(
            &dir,
            blobs.clone(),
            sourcegull_codeintel_scip::cache::DEFAULT_CACHE_BYTES,
        )
        .with_context(|| format!("creating the code graph cache in {}", dir.display()))?;
        state = state.with_graph_cache(Arc::new(graphs));
    }
    // Bulk change specs executed with `autoApplyEnabled` are applied as their creator.
    state = crate::features::install_auto_applier(state);
    state.readiness.set_migrations_current(schema_current);
    state.readiness.set_config_loaded(true);

    // Bind before spawning anything so a busy port fails startup immediately.
    let internal = sourcegull_http_api::bind(args.internal_addr)
        .await
        .with_context(|| format!("binding the internal listener on {}", args.internal_addr))?;
    let public = if roles.contains(Role::Web) {
        Some(
            sourcegull_http_api::bind(args.http_addr)
                .await
                .with_context(|| format!("binding the public listener on {}", args.http_addr))?,
        )
    } else {
        None
    };
    let mut repo_grpc = if roles.contains(Role::Repo) {
        Some(
            tokio::net::TcpListener::bind(args.repo.repo_grpc_addr)
                .await
                .with_context(|| {
                    format!(
                        "binding the repo node gRPC listener on {}",
                        args.repo.repo_grpc_addr
                    )
                })?,
        )
    } else {
        None
    };

    let mut tasks: JoinSet<(&'static str, anyhow::Result<()>)> = JoinSet::new();
    {
        let readiness = state.readiness.clone();
        let cancel = cancel.clone();
        let delay = args.shutdown_delay;
        tasks.spawn(async move {
            tokio::select! {
                () = signal.cancelled() => {
                    // `/readyz` fails first, so load balancers stop sending requests before
                    // the listeners close.
                    readiness.set_shutting_down();
                    if !delay.is_zero() {
                        tracing::info!(?delay, "not ready; shutting down after the delay");
                        tokio::select! {
                            () = tokio::time::sleep(delay) => {}
                            () = cancel.cancelled() => {}
                        }
                    }
                }
                () = cancel.cancelled() => readiness.set_shutting_down(),
            }
            tracing::info!("shutting down");
            cancel.cancel();
            ("shutdown", Ok(()))
        });
    }
    tasks.spawn(named(
        "internal listener",
        serve(
            internal,
            if roles.contains(Role::Blob) {
                // The blob role serves the local store to the other processes.
                sourcegull_http_api::internal_router(state.clone())
                    .merge(sourcegull_blobstore::router(blobs.clone()))
            } else {
                sourcegull_http_api::internal_router(state.clone())
            },
            cancel.clone(),
        ),
    ));
    if let Some(listener) = public {
        tasks.spawn(named(
            "public listener",
            serve(listener, public_router(&state), cancel.clone()),
        ));
    }
    tasks.spawn(named("connection pool gauges", {
        let (db, cancel) = (db.clone(), cancel.clone());
        async move {
            crate::metrics_collector::run_pool_gauges(db, "main", cancel).await;
            Ok(())
        }
    }));
    tasks.spawn(named("site config reloader", {
        let cancel = cancel.clone();
        async move {
            loader.run(cancel).await;
            Ok(())
        }
    }));
    if let Some(ring) = keyring.clone() {
        tasks.spawn(named("encryption key reloader", {
            let (db, cancel) = (db.clone(), cancel.clone());
            let source = key_source.clone();
            async move {
                crate::keys::run_reloader(db, ring, source, cancel).await;
                Ok(())
            }
        }));
    }
    if roles.contains(Role::Web) {
        tasks.spawn(named("permissions listener", {
            let checker = state.authz.clone();
            let cancel = cancel.clone();
            async move {
                checker.run_listener(cancel).await;
                Ok(())
            }
        }));
    }
    if (roles.contains(Role::Web) || roles.contains(Role::Worker)) && !topology.nodes.is_empty() {
        tasks.spawn(named("repo routing table", {
            let db = db.clone();
            let clients = repo_clients.clone();
            let nodes = topology.nodes.clone();
            let cancel = cancel.clone();
            async move {
                crate::routing::run_refresher(db, clients, nodes, cancel).await;
                Ok(())
            }
        }));
    }
    for role in roles.iter().filter(|r| *r != Role::Web) {
        match role {
            Role::Repo => {
                let listener = repo_grpc.take().context("repo gRPC listener")?;
                let node = sourcegull_repo_node::RepoNode::open(
                    db.clone(),
                    keyring.clone(),
                    config_handle.clone(),
                    repo_node_config(args, &topology, &config_handle)?,
                )?;
                tasks.spawn(named("repo", node.run(listener, cancel.clone())));
            }
            Role::Worker => {
                let leases = leases(args, &db).await?;
                // Next to other roles the worker gets its own pool, so background jobs cannot
                // starve requests (and the other way round).
                let worker_db = args
                    .worker_db_options(roles)
                    .map_or_else(|| db.clone(), |opts| db.sibling(&opts));
                if args.worker_db_options(roles).is_some() {
                    tasks.spawn(named("worker pool gauges", {
                        let (db, cancel) = (worker_db.clone(), cancel.clone());
                        async move {
                            crate::metrics_collector::run_pool_gauges(db, "worker", cancel).await;
                            Ok(())
                        }
                    }));
                }
                tasks.spawn(named(
                    "worker",
                    crate::worker::run(
                        worker_db,
                        config_handle.clone(),
                        keyring.clone(),
                        blobs.clone(),
                        args.data_dir.join("tmp").join("codeintel"),
                        (!topology.nodes.is_empty()).then(|| repo_clients.clone()),
                        sourcegull_declarative::Files {
                            resources: crate::cli::non_empty(&args.resources_file),
                            global_settings: crate::cli::non_empty(&args.global_settings_file),
                        },
                        leases,
                        cancel.clone(),
                    ),
                ));
            }
            _ => {
                tasks.spawn(named(role.as_str(), idle_role(role, cancel.clone())));
            }
        }
    }
    tracing::info!(%roles, "sourcegull started");

    let mut first_error = None;
    while let Some(joined) = tasks.join_next().await {
        let (name, result) = match joined {
            Ok(r) => r,
            Err(e) => ("task", Err(anyhow::anyhow!("panicked or aborted: {e}"))),
        };
        if let Err(e) = result {
            tracing::error!(
                component = name,
                error = format!("{e:#}"),
                "component failed; shutting down"
            );
            cancel.cancel();
            first_error.get_or_insert(e.context(format!("{name} failed")));
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => {
            tracing::info!("stopped");
            Ok(())
        }
    }
}

/// The public listener's router: health, UI, auth (`/-/…`), GraphQL and the features' routes
/// ([`crate::features::all`]) with the shared middleware.
pub fn public_router(state: &AppState) -> axum::Router {
    let mut routes = sourcegull_http_api::public_routes()
        .merge(sourcegull_http_api::auth_routes())
        .merge(crate::features::graphql_routes(state));
    for feature in crate::features::all() {
        if let Some(r) = feature.routes() {
            routes = routes.merge(r);
        }
    }
    sourcegull_http_api::finish(routes, state.clone())
}

async fn named<F>(name: &'static str, fut: F) -> (&'static str, anyhow::Result<()>)
where
    F: Future<Output = anyhow::Result<()>>,
{
    (name, fut.await)
}

async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    Ok(sourcegull_http_api::serve(listener, router, cancel).await?)
}

/// The repo node settings from the command line, the topology and the site config.
fn repo_node_config(
    args: &ServerArgs,
    topology: &sourcegull_repo_node::Topology,
    config: &sourcegull_config::SiteConfigHandle,
) -> anyhow::Result<sourcegull_repo_node::RepoNodeConfig> {
    let me = topology
        .me
        .clone()
        .context("the repo role needs this node's address")?;
    let data_dir = args
        .repo
        .repo_data_dir
        .clone()
        .unwrap_or_else(|| args.data_dir.clone());
    let mut cfg = sourcegull_repo_node::RepoNodeConfig::new(me, topology.nodes.clone(), data_dir);
    cfg.store.max_concurrent_clones =
        (config.load().value.git_max_concurrent_clones as usize).max(1);
    cfg.store.max_concurrent_fetches = args.repo.repo_max_concurrent_fetches.max(1);
    cfg.update_concurrency = cfg.store.max_concurrent_clones + cfg.store.max_concurrent_fetches;
    cfg.index = !args.repo.repo_disable_index;
    cfg.index_debounce = std::time::Duration::from_millis(args.repo.repo_index_debounce_ms);
    cfg.rebalance_concurrency = args.repo.repo_rebalance_concurrency;
    cfg.disk = sourcegull_gitstore::janitor::DiskWatermarks {
        pause_clones_below: args.repo.repo_pause_clones_free_percent / 100.0,
        pause_clones_below_bytes: args.repo.repo_pause_clones_free_gib.saturating_mul(1 << 30),
        evict_below: args.repo.repo_evict_free_percent / 100.0,
        evict_below_bytes: args.repo.repo_evict_free_gib.saturating_mul(1 << 30),
    };
    cfg.ssh = args
        .repo
        .repo_ssh_key
        .clone()
        .map(|private_key| sourcegull_repo_node::SshKey {
            private_key,
            known_hosts: args.repo.repo_ssh_known_hosts.clone(),
        });
    Ok(cfg)
}

/// Installs the process-wide code host rate limiters: shared through Postgres unless configured
/// (or, by default, running as a single process) otherwise.
fn install_rate_limiters(args: &ServerArgs, roles: &RoleSet, db: &Db) {
    use crate::cli::RateLimitBackend;
    let single_process = [Role::Web, Role::Repo, Role::Worker]
        .into_iter()
        .all(|r| roles.contains(r));
    let shared = match args.shared.rate_limit_backend {
        RateLimitBackend::Postgres => true,
        RateLimitBackend::Memory => false,
        RateLimitBackend::Auto => !single_process,
    };
    if shared {
        sourcegull_codehost::ratelimit::set_global(Arc::new(
            sourcegull_ratelimit::PgRateLimiters::new(db.pool().clone()),
        ));
    }
    tracing::info!(
        backend = if shared { "postgres" } else { "memory" },
        "code host rate limiters"
    );
}

/// Where the worker's singleton leases live.
async fn leases(args: &ServerArgs, db: &Db) -> anyhow::Result<sourcegull_leader::Leases> {
    use crate::cli::LeaderBackend;
    let a = &args.shared;
    Ok(match a.leader_backend {
        LeaderBackend::Postgres => sourcegull_leader::Leases::postgres(db.pool()),
        LeaderBackend::Kubernetes => {
            let identity = a.leader_identity.clone().unwrap_or_else(|| {
                let suffix: u32 = rand::random();
                format!(
                    "{}_{suffix:08x}",
                    hostname().unwrap_or_else(|| "sourcegull".to_owned())
                )
            });
            let k = sourcegull_leader::kubernetes::KubernetesLeases::infer(
                a.leader_namespace.clone(),
                a.leader_lease_prefix.clone(),
                identity,
            )
            .await
            .context("SOURCEGULL_LEADER_BACKEND=kubernetes")?
            .with_duration(std::time::Duration::from_secs(a.leader_lease_duration));
            tracing::info!(
                namespace = k.namespace(),
                identity = k.identity(),
                "singleton leases on Kubernetes"
            );
            sourcegull_leader::Leases::Kubernetes(k)
        }
    })
}

/// This machine's host name (`$HOSTNAME`, else `/etc/hostname`).
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

/// Roles without a background task of their own: `blob` serves the blob store from the internal
/// listener (see above), so its task only waits for shutdown.
async fn idle_role(role: Role, cancel: CancellationToken) -> anyhow::Result<()> {
    tracing::info!(role = %role, "role served by the internal listener; no background task");
    cancel.cancelled().await;
    Ok(())
}
