//! Client helper: one lazily connected channel per repo node, picked by the
//! [`Router`].
//!
//! Channels are HTTP/2 connections multiplexing every call to a node, so one
//! per node is enough. They connect on first use and reconnect on their own.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sourcegull_types::RepoId;
use tonic::transport::{Channel, Endpoint};

use crate::repo::v1::repo_service_client::RepoServiceClient;
use crate::repo::v1::search_service_client::SearchServiceClient;
use crate::routing::Router;

/// Settings for connections to repo nodes.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Default per-call timeout (callers can still set a shorter gRPC deadline).
    pub request_timeout: Option<Duration>,
    /// HTTP/2 keep-alive ping interval.
    pub keepalive: Duration,
    /// Largest message accepted from a node (file contents, diff batches).
    pub max_decoding_message_size: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: None,
            keepalive: Duration::from_secs(30),
            max_decoding_message_size: 64 * 1024 * 1024,
        }
    }
}

/// Errors from [`RepoClients`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No repo nodes are configured.
    #[error("no repo node configured for repo {0}")]
    NoNode(RepoId),
    /// The node address is not a valid URI.
    #[error("invalid repo node address {addr:?}: {message}")]
    InvalidAddress {
        /// The rejected address.
        addr: String,
        /// Parser message.
        message: String,
    },
}

/// Routes calls to the node that owns a repo, caching one channel per node.
///
/// Cheap to clone; clones share the channel cache. Swap the routing table with
/// [`RepoClients::set_router`] when the node list or pins change.
#[derive(Clone)]
pub struct RepoClients {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RepoClients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoClients")
            .field("router", &self.router())
            .finish_non_exhaustive()
    }
}

struct Inner {
    config: ClientConfig,
    router: RwLock<Arc<Router>>,
    channels: RwLock<HashMap<String, Channel>>,
}

impl RepoClients {
    /// Creates a client set over `router`.
    pub fn new(router: Router, config: ClientConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                router: RwLock::new(Arc::new(router)),
                channels: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// The current routing table.
    pub fn router(&self) -> Arc<Router> {
        self.inner
            .router
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replaces the routing table. Channels to nodes that left are dropped.
    pub fn set_router(&self, router: Router) {
        let keep: Vec<String> = router.nodes().map(str::to_owned).collect();
        *self.inner.router.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(router);
        self.inner
            .channels
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|addr, _| keep.contains(addr));
    }

    /// A client for the node that owns `repo`.
    pub fn for_repo(&self, repo: RepoId) -> Result<RepoServiceClient<Channel>, ClientError> {
        let router = self.router();
        let addr = router.node_for(repo).ok_or(ClientError::NoNode(repo))?;
        self.for_node(addr)
    }

    /// A client for the node at `addr`.
    pub fn for_node(&self, addr: &str) -> Result<RepoServiceClient<Channel>, ClientError> {
        let channel = self.channel(addr)?;
        Ok(RepoServiceClient::new(channel)
            .max_decoding_message_size(self.inner.config.max_decoding_message_size))
    }

    /// A search client for the node that owns `repo`.
    pub fn search_for_repo(
        &self,
        repo: RepoId,
    ) -> Result<SearchServiceClient<Channel>, ClientError> {
        let router = self.router();
        let addr = router.node_for(repo).ok_or(ClientError::NoNode(repo))?;
        self.search_for_node(addr)
    }

    /// A search client for the node at `addr`.
    pub fn search_for_node(&self, addr: &str) -> Result<SearchServiceClient<Channel>, ClientError> {
        let channel = self.channel(addr)?;
        Ok(SearchServiceClient::new(channel)
            .max_decoding_message_size(self.inner.config.max_decoding_message_size))
    }

    fn channel(&self, addr: &str) -> Result<Channel, ClientError> {
        if let Some(ch) = self
            .inner
            .channels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(addr)
        {
            return Ok(ch.clone());
        }
        let channel = connect_lazy(addr, &self.inner.config)?;
        let mut channels = self
            .inner
            .channels
            .write()
            .unwrap_or_else(|e| e.into_inner());
        Ok(channels.entry(addr.to_owned()).or_insert(channel).clone())
    }
}

/// Builds a lazily connecting channel to `addr` with `config` applied.
pub fn connect_lazy(addr: &str, config: &ClientConfig) -> Result<Channel, ClientError> {
    let mut endpoint =
        Endpoint::from_shared(addr.to_owned()).map_err(|e| ClientError::InvalidAddress {
            addr: addr.to_owned(),
            message: e.to_string(),
        })?;
    endpoint = endpoint
        .connect_timeout(config.connect_timeout)
        .http2_keep_alive_interval(config.keepalive)
        .keep_alive_while_idle(true)
        .tcp_nodelay(true);
    if let Some(t) = config.request_timeout {
        endpoint = endpoint.timeout(t);
    }
    Ok(endpoint.connect_lazy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caches_channels_per_node_and_routes() {
        let clients = RepoClients::new(
            Router::new(["http://127.0.0.1:1", "http://127.0.0.1:2"]),
            ClientConfig::default(),
        );
        for id in 0..20 {
            clients.for_repo(RepoId(id)).unwrap();
        }
        assert_eq!(clients.inner.channels.read().unwrap().len(), 2);
        clients.set_router(Router::new(["http://127.0.0.1:1"]));
        assert_eq!(clients.inner.channels.read().unwrap().len(), 1);

        let empty = RepoClients::new(Router::default(), ClientConfig::default());
        assert!(matches!(
            empty.for_repo(RepoId(1)),
            Err(ClientError::NoNode(RepoId(1)))
        ));
        assert!(matches!(
            clients.for_node("not a uri"),
            Err(ClientError::InvalidAddress { .. })
        ));
    }
}
