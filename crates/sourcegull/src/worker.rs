//! The `worker` role: background jobs and leader-elected singleton tasks.
//!
//! Singletons run under [`sourcegull_leader::run_singleton`] with a lease from [`Leases`] (a
//! Postgres advisory lock, all on one shared session, or a Kubernetes `Lease` with `SOURCEGULL_LEADER_BACKEND=kubernetes`), so
//! exactly one worker in the deployment runs each of them. Queue workers and resetters run in
//! every worker process.
//!
//! The features' workers and singletons come from [`crate::features::all`]
//! ([`sourcegull_feature::Feature::worker_tasks`]); work that reads repositories starts only when
//! repository nodes are configured. Besides them the role runs:
//! - the session janitor singleton (also deletes expired OAuth sign-in flows);
//! - the configuration-as-code reconciler singleton ([`sourcegull_declarative::Reconciler`]), which
//!   applies `SOURCEGULL_RESOURCES_FILE` and `SOURCEGULL_GLOBAL_SETTINGS_FILE`; without them it releases
//!   what an earlier configuration managed;
//! - for `codeintel_index_jobs` and `batch_spec_workspace_execution_jobs` (the `codeintel` and
//!   `batches` executor queues): no worker, executors take the jobs over HTTP; a resetter per queue
//!   returns jobs of executors that stopped heartbeating, and executors not seen for a day are
//!   dropped from the admin list;
//! - the refresh of the domain gauges read from Postgres ([`crate::metrics_collector`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use sourcegull_authn::PgSessionStore;
use sourcegull_blobstore::Blobs;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::Db;
use sourcegull_feature::{WorkerContext, WorkerTasks};
use sourcegull_leader::{DEFAULT_RENEW_INTERVAL, Leases, run_singleton};
use sourcegull_rpc::RepoClients;
use sourcegull_types::metric_names;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Lease name of the session janitor.
pub const SESSION_JANITOR: &str = "session-janitor";
/// How often the session janitor deletes expired sessions.
pub const SESSION_JANITOR_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Counter of expired sessions deleted.
pub const SESSIONS_DELETED_METRIC: &str = metric_names::SESSIONS_EXPIRED_DELETED_TOTAL;

/// The features' queue workers and singletons for one worker process.
pub fn feature_tasks(ctx: &WorkerContext) -> anyhow::Result<WorkerTasks> {
    let mut tasks = WorkerTasks::default();
    for feature in crate::features::all() {
        feature
            .worker_tasks(ctx, &mut tasks)
            .with_context(|| format!("starting the {} feature's workers", feature.name()))?;
    }
    Ok(tasks)
}

/// Runs the worker's singletons and queue workers until `cancel` fires.
///
/// `keyring` decrypts code host connection configs; without it, syncs fail with a clear error.
/// `codeintel_dir` holds temporary files of upload processing. Without `repo_clients` (no
/// repository nodes configured), work that reads repositories does not run and upload commits
/// are not checked. `leases` holds the singletons' leases. `declared` names the
/// configuration-as-code files.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    db: Db,
    config: SiteConfigHandle,
    keyring: Option<Arc<Keyring>>,
    blobs: Blobs,
    codeintel_dir: PathBuf,
    repo_clients: Option<Arc<RepoClients>>,
    declared: sourcegull_declarative::Files,
    leases: Leases,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    tracing::info!("worker started");
    let mut tasks = JoinSet::new();
    {
        let reconciler = sourcegull_declarative::Reconciler::new(
            db.clone(),
            keyring.clone(),
            declared,
            Some(config.clone()),
        );
        let lease = leases.lease(sourcegull_declarative::RECONCILER);
        tasks.spawn(run_singleton(
            sourcegull_declarative::RECONCILER,
            lease,
            Some(db.pool().clone()),
            DEFAULT_RENEW_INTERVAL,
            move |token, _fence| reconciler.clone().run(token),
            cancel.clone(),
        ));
    }
    {
        let db = db.clone();
        let lease = leases.lease(SESSION_JANITOR);
        tasks.spawn(run_singleton(
            SESSION_JANITOR,
            lease,
            Some(db.pool().clone()),
            DEFAULT_RENEW_INTERVAL,
            move |token, _fence| session_janitor(db.clone(), token),
            cancel.clone(),
        ));
    }
    let ctx = WorkerContext {
        db: db.clone(),
        config: config.clone(),
        keyring,
        blobs,
        codeintel_dir,
        repo_clients,
        worker_id: sourcegull_pgqueue::default_worker_id(),
    };
    let features = feature_tasks(&ctx)?;
    for singleton in features.singletons {
        let lease = leases.lease(singleton.name);
        tasks.spawn(run_singleton(
            singleton.name,
            lease,
            Some(db.pool().clone()),
            DEFAULT_RENEW_INTERVAL,
            singleton.run,
            cancel.clone(),
        ));
    }
    for worker in features.workers {
        tasks.spawn((worker.run)(cancel.clone()));
    }
    {
        let queue: sourcegull_pgqueue::Queue<sourcegull_codeintel_scip::store::IndexJob> =
            sourcegull_pgqueue::Queue::new(
                db.pool().clone(),
                sourcegull_codeintel_scip::store::IndexJobStore::queue_config(),
            )?;
        tasks.spawn(
            sourcegull_pgqueue::Resetter::new(queue, Duration::from_secs(30)).run(cancel.clone()),
        );
        let queue: sourcegull_pgqueue::Queue<sourcegull_batches::store::ExecutionJob> =
            sourcegull_pgqueue::Queue::new(
                db.pool().clone(),
                sourcegull_batches::store::BatchSpecWorkspaceStore::execution_queue_config(
                    sourcegull_batches::store::EXECUTION_JOB_VERSION,
                ),
            )?;
        tasks.spawn(
            sourcegull_pgqueue::Resetter::new(queue, Duration::from_secs(30)).run(cancel.clone()),
        );
        tasks.spawn(prune_executors(db.clone(), cancel.clone()));
        // Domain gauges for the dashboards and alerts.
        tasks.spawn(crate::metrics_collector::run(db, config, cancel.clone()));
    }
    while let Some(joined) = tasks.join_next().await {
        if let Err(e) = joined {
            anyhow::bail!("worker task panicked: {e}");
        }
    }
    tracing::info!("worker stopped");
    Ok(())
}

/// Deletes expired sessions and OAuth flows every [`SESSION_JANITOR_INTERVAL`] (first run right
/// away) until `cancel` fires.
async fn session_janitor(db: Db, cancel: CancellationToken) {
    let sessions = PgSessionStore::new(db.clone());
    let mut tick = tokio::time::interval(SESSION_JANITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(SESSION_JANITOR, sessions.delete_expired()).await {
            Ok(n) => {
                metrics::counter!(SESSIONS_DELETED_METRIC).increment(n);
                if n > 0 {
                    tracing::info!(deleted = n, "deleted expired sessions");
                }
            }
            Err(e) => tracing::warn!(error = %e, "deleting expired sessions failed"),
        }
        if let Err(e) = db.external_accounts(None).delete_expired_flows().await {
            tracing::warn!(error = %e, "deleting expired OAuth flows failed");
        }
    }
}

/// Drops executors not seen for a day from `executor_heartbeats`, hourly.
async fn prune_executors(db: Db, cancel: CancellationToken) {
    loop {
        let before = chrono::Utc::now() - chrono::Duration::days(1);
        if let Err(e) = db.executors().delete_stale(before).await {
            tracing::warn!(error = %e, "pruning executors failed");
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
}
