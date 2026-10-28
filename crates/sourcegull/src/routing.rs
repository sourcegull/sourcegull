//! The web side's repo → node routing table: a [`Router`] over the configured repo nodes, with the
//! persisted assignments (`repo.shard`) that differ from the hash applied as pins. Loaded at start
//! and kept current on `NOTIFY repo_shards`: a notification naming repos reloads only those
//! ([`reload_repos`]); an empty one, a missed one (a resync) and a periodic poll reload them all.

use std::sync::Arc;
use std::time::Duration;

use sourcegull_db::Db;
use sourcegull_db::notify::{channels, parse_repo_shards_payload};
use sourcegull_rpc::{ClientConfig, RepoClients, Router};
use sourcegull_types::RepoId;
use tokio_util::sync::CancellationToken;

/// Poll fallback for missed notifications.
pub const ROUTING_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Notifications arriving within this window are folded into one reload.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Repo node clients over `nodes`, without pins (call [`reload`] to add them).
pub fn repo_clients(nodes: &[String]) -> Arc<RepoClients> {
    Arc::new(RepoClients::new(
        Router::new(nodes.iter().cloned()),
        ClientConfig::default(),
    ))
}

/// Builds the routing table from `nodes` and the persisted assignments and installs it.
/// Returns the number of pinned repos.
pub async fn reload(
    db: &Db,
    clients: &RepoClients,
    nodes: &[String],
) -> Result<usize, sourcegull_db::DbError> {
    let mut router = Router::new(nodes.iter().cloned());
    let mut pins = 0;
    for (repo, shard) in db.repos().shards().await? {
        if router.hashed_node_for(repo) != Some(shard.as_str()) {
            router.pin(repo, shard);
            pins += 1;
        }
    }
    clients.set_router(router);
    Ok(pins)
}

/// Reloads the persisted assignments of `repos` into the installed routing table.
pub async fn reload_repos(
    db: &Db,
    clients: &RepoClients,
    repos: &[RepoId],
) -> Result<(), sourcegull_db::DbError> {
    let shards = db.repos().shards_of(repos).await?;
    let mut router = (*clients.router()).clone();
    for (repo, shard) in shards {
        match shard {
            Some(shard) if router.hashed_node_for(repo) != Some(shard.as_str()) => {
                router.pin(repo, shard);
            }
            _ => router.unpin(repo),
        }
    }
    clients.set_router(router);
    Ok(())
}

/// What the next reload covers.
enum Pending {
    /// These repos.
    Repos(Vec<RepoId>),
    /// Every repo.
    All,
}

impl Pending {
    fn add(&mut self, event: &sourcegull_pubsub::Event) {
        let ids = match event {
            sourcegull_pubsub::Event::Notification { payload, .. } => {
                parse_repo_shards_payload(payload)
            }
            sourcegull_pubsub::Event::Resync => None,
        };
        match (&mut *self, ids) {
            (Pending::Repos(have), Some(ids)) => have.extend(ids),
            (_, None) => *self = Pending::All,
            (Pending::All, Some(_)) => {}
        }
    }
}

/// Keeps `clients`' routing table current until `cancel` fires.
pub async fn run_refresher(
    db: Db,
    clients: Arc<RepoClients>,
    nodes: Vec<String>,
    cancel: CancellationToken,
) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(db.pool(), &[channels::REPO_SHARDS]) => s,
    };
    loop {
        let mut pending = Pending::Repos(Vec::new());
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(ROUTING_POLL_INTERVAL) => pending = Pending::All,
            e = sub.recv() => {
                pending.add(&e);
                // Fold a burst of notifications into one reload; a resync reloads right away.
                if matches!(e, sourcegull_pubsub::Event::Notification { .. }) {
                    let until = tokio::time::Instant::now() + DEBOUNCE;
                    while let Ok(e) = tokio::time::timeout_at(until, sub.recv()).await {
                        pending.add(&e);
                    }
                }
            }
        }
        match pending {
            Pending::All => match reload(&db, &clients, &nodes).await {
                Ok(pins) => tracing::debug!(pins, "repo routing table reloaded"),
                Err(e) => tracing::warn!(error = %e, "reloading the repo routing table failed"),
            },
            Pending::Repos(mut repos) => {
                repos.sort_unstable();
                repos.dedup();
                match reload_repos(&db, &clients, &repos).await {
                    Ok(()) => tracing::debug!(repos = repos.len(), "repo routes reloaded"),
                    Err(e) => {
                        tracing::warn!(error = %e, "reloading repo routes failed");
                        // The next notification or poll may come late; reload everything now.
                        if let Err(e) = reload(&db, &clients, &nodes).await {
                            tracing::warn!(error = %e, "reloading the repo routing table failed");
                        }
                    }
                }
            }
        }
    }
}
