//! Feature registration.
//!
//! Each feature crate implements [`Feature`] and the `sourcegull` binary keeps one list of them. The
//! `web` role merges every feature's [`Feature::routes`] into the public router; the `worker`
//! role asks every feature for its tasks ([`Feature::worker_tasks`]) and runs them: queue workers
//! and resetters in every worker process, singletons under a lease of their name so exactly one
//! worker in the deployment runs each.
//!
//! GraphQL types and resolvers are not registered here: the `graphql` crate assembles the schema.
//! A feature's GraphQL contribution is meant to become another method of [`Feature`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use sourcegull_blobstore::Blobs;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, Fence};
use sourcegull_rpc::RepoClients;
use tokio_util::sync::CancellationToken;

/// A boxed task future.
pub type BoxTask = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A part of the product that contributes routes and background work to the server roles.
///
/// `S` is the state of the web role's router. Features without routes implement it for every
/// `S`.
pub trait Feature<S>: Send + Sync {
    /// Short name, for logs.
    fn name(&self) -> &'static str;

    /// Routes merged into the public router of the `web` role.
    fn routes(&self) -> Option<axum::Router<S>> {
        None
    }

    /// Adds the feature's queue workers and singletons for one worker process to `tasks`.
    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let _ = (ctx, tasks);
        Ok(())
    }
}

/// What the `worker` role hands every feature.
#[derive(Clone)]
pub struct WorkerContext {
    /// The worker's pool.
    pub db: Db,
    pub config: SiteConfigHandle,
    /// Decrypts code host connection configs and secrets; `None` without keys.
    pub keyring: Option<Arc<Keyring>>,
    pub blobs: Blobs,
    /// Temporary files of code intelligence processing.
    pub codeintel_dir: PathBuf,
    /// The repository nodes; `None` when none are configured. Work that reads repositories is
    /// not started without them.
    pub repo_clients: Option<Arc<RepoClients>>,
    /// This process's queue worker id.
    pub worker_id: String,
}

/// A task run in every worker process until its token is cancelled.
pub struct Worker {
    pub run: Box<dyn FnOnce(CancellationToken) -> BoxTask + Send>,
}

/// A task run by one worker of the deployment, under the lease `name`. `run` is called each time
/// the lease is acquired, with the lease's [`Fence`] for the writes a leader that lost the lease
/// must not make; the token is cancelled when the lease is lost or on shutdown.
pub struct Singleton {
    pub name: &'static str,
    pub run: Box<dyn FnMut(CancellationToken, Fence) -> BoxTask + Send>,
}

/// The tasks the features add, in the order they add them.
#[derive(Default)]
pub struct WorkerTasks {
    pub workers: Vec<Worker>,
    pub singletons: Vec<Singleton>,
}

impl WorkerTasks {
    /// Adds a task for every worker process (a queue worker, a resetter).
    pub fn worker<F, Fut>(&mut self, run: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.workers.push(Worker {
            run: Box::new(move |token| Box::pin(run(token))),
        });
    }

    /// Adds a leader-elected task under the lease `name`.
    pub fn singleton<F, Fut>(&mut self, name: &'static str, mut run: F)
    where
        F: FnMut(CancellationToken, Fence) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.singletons.push(Singleton {
            name,
            run: Box::new(move |token, fence| Box::pin(run(token, fence))),
        });
    }

    /// The singletons' lease names.
    pub fn singleton_names(&self) -> Vec<&'static str> {
        self.singletons.iter().map(|s| s.name).collect()
    }
}
