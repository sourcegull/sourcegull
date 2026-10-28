//! The `repo` role: a repo node serving the git storage gRPC service and keeping its partition of
//! repositories cloned and current.
//!
//! A [`RepoNode`] runs, until shutdown:
//! - the `sourcegull.repo.v1.RepoService` gRPC server over a [`GitStore`], resolving remotes from
//!   the database ([`DbResolver`]);
//! - the `repo_update_jobs` consumer ([`consumer`]): bounded concurrency, `LISTEN/NOTIFY`
//!   wake-ups with a poll fallback, heartbeats, and a resetter for stalled jobs;
//! - the scheduler ([`scheduler`]): persists the hash assignment of unassigned repos in
//!   `repo.shard` and enqueues LOW-priority periodic fetches;
//! - the janitor ([`janitor`]): maintenance, corruption → re-clone, stale temp cleanup,
//!   removal of deleted repos, disk pressure;
//! - the indexer ([`indexing`]): the search index over this node's repos, rebuilt after fetches
//!   move refs, and served as `SearchIndexed` next to unindexed search;
//! - the rebalancer ([`rebalance`]): heartbeats, and the copy-then-flip moves of repos between
//!   nodes when nodes are added or removed.

// The repo node is internal: it reads the repositories it stores for every viewer, and `web`
// applies permissions to what it asks for (clippy.toml).
#![allow(clippy::disallowed_methods)]

mod consumer;
mod indexing;
mod janitor;
pub mod rebalance;
mod rebalance_plan;
pub mod resolver;
mod scheduler;
pub mod topology;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context as _;
use sourcegull_config::SiteConfigHandle;
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, RepoUpdateJob, RepoUpdateJobStore};
use sourcegull_gitstore::janitor::{DiskWatermarks, MaintenanceThresholds};
use sourcegull_gitstore::{GitStore, GitStoreConfig, RepoNodeService, ServiceConfig};
use sourcegull_pgqueue::{Queue, Resetter, Worker, WorkerOptions};
use sourcegull_repos::Registry;
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub use janitor::JanitorReport;
pub use rebalance::{MoveOutcome, PlanReport};
pub use resolver::{DbResolver, ResolveError, SshKey};
pub use topology::Topology;

/// Default gRPC port of repo nodes.
pub const DEFAULT_GRPC_PORT: u16 = 3178;

/// Settings of a repo node.
#[derive(Debug, Clone)]
pub struct RepoNodeConfig {
    /// This node's address, as listed in [`nodes`](Self::nodes).
    pub me: String,
    /// Every repo node's address.
    pub nodes: Vec<String>,
    /// Git storage settings (data directory, concurrency, timeouts).
    pub store: GitStoreConfig,
    /// Update jobs run at once. Clones and fetches are additionally bounded by the store.
    pub update_concurrency: usize,
    /// Queue poll interval when no notification arrives.
    pub poll_interval: Duration,
    /// Scheduler pass interval.
    pub scheduler_interval: Duration,
    /// Janitor pass interval.
    pub janitor_interval: Duration,
    /// Delay before the first janitor pass after start.
    pub janitor_initial_delay: Duration,
    /// How long a deleted repo stays on disk before the janitor removes it.
    pub deleted_grace: Duration,
    /// Allow `file://` and local-path clone and push URLs (tests and development only).
    pub allow_local_remotes: bool,
    /// SSH key for `gitURLType: ssh` connections.
    pub ssh: Option<SshKey>,
    /// When the janitor repacks.
    pub thresholds: MaintenanceThresholds,
    /// Disk watermarks (pause clones when free space is low).
    pub disk: DiskWatermarks,
    /// Keep a search index of this node's repos.
    pub index: bool,
    /// Debounce window of reindexes after fetches that moved refs: a repo not built within
    /// the window is built at once; pushes within the window of a build coalesce into one
    /// build at its end.
    pub index_debounce: Duration,
    /// Interval of the pass that queues every repo on disk for an index check.
    pub index_reconcile_interval: Duration,
    /// Repo moves to this node run at once; 0 turns moves to this node and planning off (heartbeats
    /// and the removal of moved repos still run).
    pub rebalance_concurrency: usize,
    /// Rebalancer pass interval: heartbeat, claiming moves, removing moved repos. A node
    /// whose heartbeat is six intervals old counts as down.
    pub rebalance_interval: Duration,
    /// Interval of the planning pass and of the removal of unneeded copies.
    pub rebalance_plan_interval: Duration,
    /// How long the old owner keeps a moved repo after the flip, for routing tables to
    /// reload and requests in flight to finish.
    pub move_gc_grace: Duration,
    /// Attempts of a move before it is marked failed.
    pub move_max_attempts: u32,
}

impl RepoNodeConfig {
    /// Defaults for node `me` among `nodes`, storing repos under `data_dir`.
    pub fn new(me: impl Into<String>, nodes: Vec<String>, data_dir: impl Into<PathBuf>) -> Self {
        let store = GitStoreConfig::new(data_dir);
        Self {
            me: me.into(),
            nodes,
            update_concurrency: store.max_concurrent_clones + store.max_concurrent_fetches,
            store,
            poll_interval: Duration::from_secs(5),
            scheduler_interval: Duration::from_secs(30),
            janitor_interval: Duration::from_secs(3600),
            janitor_initial_delay: Duration::from_secs(120),
            deleted_grace: Duration::from_secs(24 * 3600),
            allow_local_remotes: false,
            ssh: None,
            thresholds: MaintenanceThresholds::default(),
            disk: DiskWatermarks::default(),
            index: true,
            index_debounce: Duration::from_secs(5),
            index_reconcile_interval: Duration::from_secs(600),
            rebalance_concurrency: 2,
            rebalance_interval: Duration::from_secs(10),
            rebalance_plan_interval: Duration::from_secs(60),
            move_gc_grace: Duration::from_secs(300),
            move_max_attempts: 5,
        }
    }
}

/// A running repo node's shared state. Cheap to clone.
#[derive(Clone)]
pub struct RepoNode {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    store: Arc<GitStore>,
    router: Router,
    me: String,
    resolver: DbResolver,
    site: SiteConfigHandle,
    cfg: RepoNodeConfig,
    clones_paused: AtomicBool,
    index: Option<Arc<dyn sourcegull_search_exec::index::SearchIndex>>,
    /// Sourcegull's own index when it is the backend (compound shard packing).
    shards: Option<Arc<sourcegull_search_exec::ShardIndex>>,
    index_queue: indexing::IndexQueue,
    /// Clients of the other repo nodes (copy sources of moves).
    peers: RepoClients,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl std::fmt::Debug for RepoNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoNode")
            .field("me", &self.inner.me)
            .field("nodes", &self.inner.cfg.nodes)
            .finish_non_exhaustive()
    }
}

impl RepoNode {
    /// Opens the git store (creating the data directory layout) and prepares the node.
    /// `keyring` decrypts code host connection configs for credentials.
    pub fn open(
        db: Db,
        keyring: Option<Arc<Keyring>>,
        site: SiteConfigHandle,
        cfg: RepoNodeConfig,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg.nodes.contains(&cfg.me),
            "repo node {:?} is not in the node list {:?}",
            cfg.me,
            cfg.nodes
        );
        let store = GitStore::open(cfg.store.clone()).with_context(|| {
            format!(
                "opening the repo data directory {}",
                cfg.store.root.display()
            )
        })?;
        let (index, shards) = if cfg.index {
            indexing::open_index(&store)?
        } else {
            (None, None)
        };
        let resolver = DbResolver::new(
            db.clone(),
            keyring,
            Registry::default(),
            cfg.ssh.clone(),
            cfg.allow_local_remotes,
        )
        .with_node(&cfg.me);
        let cfg_nodes = cfg.nodes.clone();
        let index_queue = indexing::IndexQueue::new(cfg.index_debounce);
        Ok(Self {
            inner: Arc::new(Inner {
                router: Router::new(cfg.nodes.iter().cloned()),
                me: cfg.me.clone(),
                store: Arc::new(store),
                db,
                resolver,
                site,
                cfg,
                clones_paused: AtomicBool::new(false),
                index,
                shards,
                index_queue,
                peers: RepoClients::new(Router::new(cfg_nodes), ClientConfig::default()),
                started_at: chrono::Utc::now(),
            }),
        })
    }

    /// This node's address.
    pub fn me(&self) -> &str {
        &self.inner.me
    }

    /// The git store.
    pub fn store(&self) -> &Arc<GitStore> {
        &self.inner.store
    }

    /// The remote resolver.
    pub fn resolver(&self) -> &DbResolver {
        &self.inner.resolver
    }

    /// The update queue as this node consumes it: priority order, only jobs for repos assigned
    /// to this node or to none.
    pub fn queue(&self) -> anyhow::Result<Queue<RepoUpdateJob>> {
        Ok(Queue::new(
            self.inner.db.pool().clone(),
            RepoUpdateJobStore::queue_config()
                .with_dequeue_filter(consumer::OWNERSHIP_FILTER, self.inner.me.clone()),
        )?)
    }

    /// Serves gRPC on `listener` and runs the queue consumer, scheduler and janitor until
    /// `cancel` fires.
    pub async fn run(
        self,
        listener: tokio::net::TcpListener,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let addr: Option<SocketAddr> = listener.local_addr().ok();
        tracing::info!(me = %self.inner.me, nodes = ?self.inner.cfg.nodes, ?addr, "repo node started");
        let queue = self.queue()?;
        let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();
        {
            let service = RepoNodeService::new(
                self.inner.store.clone(),
                self.inner.resolver.clone(),
                ServiceConfig {
                    // Bulk changes pushes go to local paths only where clones may come from them.
                    allow_local_push: self.inner.cfg.allow_local_remotes,
                    disk: self.inner.cfg.disk,
                    ..ServiceConfig::default()
                },
            )
            .into_server();
            let site = self.inner.site.clone();
            let mut search =
                sourcegull_search_exec::SearchNodeService::new(self.inner.store.clone())
                    .with_always_index(Arc::new(move |repo: &str| {
                        site.load().value.always_index(repo)
                    }));
            if let Some(index) = &self.inner.index {
                search = search
                    .with_index(index.clone())
                    .with_control(Arc::new(self.clone()));
            }
            let search = search.into_server();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                sourcegull_rpc::tonic::transport::Server::builder()
                    .add_service(service)
                    .add_service(search)
                    .serve_with_incoming_shutdown(
                        sourcegull_rpc::incoming(listener),
                        cancel.cancelled_owned(),
                    )
                    .await
                    .context("repo node gRPC server")
            });
        }
        {
            let mut opts = WorkerOptions::new(format!(
                "{}-{}",
                self.inner.me,
                sourcegull_pgqueue::default_worker_id()
            ));
            opts.concurrency = self.inner.cfg.update_concurrency.max(1);
            opts.poll_interval = self.inner.cfg.poll_interval;
            let worker = Worker::new(
                queue.clone(),
                consumer::UpdateHandler { node: self.clone() },
                opts,
            );
            let worker_cancel = cancel.clone();
            tasks.spawn(async move {
                worker.run(worker_cancel).await;
                Ok(())
            });
            let resetter = Resetter::new(queue, Duration::from_secs(30));
            let resetter_cancel = cancel.clone();
            tasks.spawn(async move {
                resetter.run(resetter_cancel).await;
                Ok(())
            });
        }
        {
            let node = self.clone();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                node.run_scheduler(cancel).await;
                Ok(())
            });
        }
        {
            let node = self.clone();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                node.run_janitor(cancel).await;
                Ok(())
            });
        }
        {
            let node = self.clone();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                node.run_indexer(cancel).await;
                Ok(())
            });
        }
        {
            let node = self.clone();
            let cancel = cancel.clone();
            tasks.spawn(async move {
                node.run_rebalancer(cancel).await;
                Ok(())
            });
        }
        let mut first_error = None;
        while let Some(joined) = tasks.join_next().await {
            let result = joined.unwrap_or_else(|e| Err(anyhow::anyhow!("task panicked: {e}")));
            if let Err(e) = result {
                cancel.cancel();
                first_error.get_or_insert(e);
            }
        }
        tracing::info!("repo node stopped");
        first_error.map_or(Ok(()), Err)
    }
}
