//! Site admin repository pages: `Repository.textSearchIndex`,
//! `repositoryStats`, `gitservers` (disk usage per repo node) and the `recloneRepository` and
//! `reindexRepository` mutations. The `repositories` filters (`corrupted`, `indexed`,
//! `notIndexed`) and `mirrorInfo` live in [`crate::repos`].
//!
//! Types: `RepositoryTextSearchIndex`, `RepositoryStats`, `GitserverInstance`, with a few
//! additions: the index's last error, its shards and whether a build is queued, and the repos and
//! shards held per node.
//!
//! Everything here is for site admins only. Live data comes from the repo nodes over gRPC
//! (`IndexStatus`, `Reindex`, `DiskInfo`); the indexer's last reported state, which also backs
//! the list filters, comes from `search_index_repos`.

use std::collections::BTreeMap;
use std::time::Duration;

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, TimeZone, Utc};
use sourcegull_db::{NodeUsage, RepoUpdatePriority, RepoUpdateReason, SearchIndexRepo};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::relay::marshal_string_id;
use sourcegull_types::{Repo, RepoId};

use crate::EmptyResponse;
use crate::context::{codes, db_error, error, require_site_admin, services, to_int};
use crate::git::{GitObjectNode, GitRefNode, RepoHandle};
use crate::pagination::PageInfo;
use crate::repos::{BigInt, RepositoryNode, parse_repository_id};

/// How long admin pages wait for a repo node before showing it as unreachable.
const NODE_TIMEOUT: Duration = Duration::from_secs(5);

/// The index name of the default branch (as the indexer uses it).
const HEAD: &str = "HEAD";

fn big(n: u64) -> BigInt {
    BigInt(i64::try_from(n).unwrap_or(i64::MAX))
}

fn int(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Calls a repo node with [`NODE_TIMEOUT`]; the error is a message for the page.
async fn call<T>(
    fut: impl Future<
        Output = std::result::Result<
            sourcegull_rpc::tonic::Response<T>,
            sourcegull_rpc::tonic::Status,
        >,
    >,
) -> std::result::Result<T, String> {
    match tokio::time::timeout(NODE_TIMEOUT, fut).await {
        Ok(Ok(r)) => Ok(r.into_inner()),
        Ok(Err(s)) => Err(s.message().to_owned()),
        Err(_) => Err(format!(
            "the repo node did not answer within {} s",
            NODE_TIMEOUT.as_secs()
        )),
    }
}

// ------------------------------------------------------------------ text search index

/// `Repository.textSearchIndex` (site admins only).
pub(crate) async fn text_search_index(
    ctx: &Context<'_>,
    repo: &Repo,
) -> Result<Option<TextSearchIndexNode>> {
    require_site_admin(ctx)?;
    let svc = services(ctx)?;
    let row = svc.db.search_index().get(repo.id).await.map_err(db_error)?;
    let host = svc
        .repo_clients
        .router()
        .node_for(repo.id)
        .map(str::to_owned);
    let extra = svc
        .config
        .load()
        .value
        .search_index_branches
        .get(&repo.name)
        .cloned()
        .unwrap_or_default();
    Ok(Some(TextSearchIndexNode {
        repo: repo.clone(),
        row,
        clients: svc.repo_clients.clone(),
        live: tokio::sync::OnceCell::new(),
        host,
        extra,
    }))
}

/// `RepositoryTextSearchIndex`.
pub(crate) struct TextSearchIndexNode {
    repo: Repo,
    /// What the indexer last reported.
    row: Option<SearchIndexRepo>,
    clients: std::sync::Arc<sourcegull_rpc::RepoClients>,
    /// What the node holds now, asked on first use (lists of repositories only read the
    /// reported state); the error when the node could not be asked.
    live: tokio::sync::OnceCell<std::result::Result<pb::IndexStatusResponse, String>>,
    host: Option<String>,
    /// `search.index.branches` of the repo.
    extra: Vec<String>,
}

impl TextSearchIndexNode {
    /// The node's live status, or why it could not be asked.
    async fn live_status(&self) -> std::result::Result<&pb::IndexStatusResponse, &str> {
        let live = self
            .live
            .get_or_init(|| async {
                let mut client = self
                    .clients
                    .search_for_repo(self.repo.id)
                    .map_err(|e| e.to_string())?;
                call(client.index_status(pb::IndexStatusRequest {
                    repo: Some(pb::RepoRef {
                        id: self.repo.id.get(),
                        name: self.repo.name.clone(),
                    }),
                }))
                .await
            })
            .await;
        live.as_ref().map_err(String::as_str)
    }

    /// The indexed branches and their commits: the node's, or the last report when the node
    /// could not be asked.
    async fn indexed_branches(&self) -> Vec<(String, String)> {
        match (self.live_status().await.ok(), &self.row) {
            (Some(l), _) => l
                .branches
                .iter()
                .map(|b| (b.name.clone(), b.commit_oid.clone()))
                .collect(),
            (None, Some(r)) if r.indexed => r
                .branches
                .iter()
                .map(|b| (b.name.clone(), b.commit.clone()))
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[Object(name = "RepositoryTextSearchIndex")]
impl TextSearchIndexNode {
    /// The indexed repository.
    async fn repository(&self) -> RepositoryNode {
        RepositoryNode {
            repo: self.repo.clone(),
            gitserver: None,
        }
    }

    /// The status of the index; null when the repository is not indexed.
    async fn status(&self) -> Option<TextSearchIndexStatus> {
        match (self.live_status().await.ok(), &self.row) {
            (Some(l), _) if l.indexed => Some(TextSearchIndexStatus {
                updated_at: (l.updated_at > 0)
                    .then(|| Utc.timestamp_opt(l.updated_at, 0).single())
                    .flatten()
                    .or(self.row.as_ref().and_then(|r| r.last_indexed_at))
                    .unwrap_or_else(Utc::now),
                content_byte_size: big(l.content_bytes),
                content_files_count: int(l.docs),
                index_byte_size: int(l.index_bytes),
                index_shards_count: int(l.shards.len() as u64),
                new_lines_count: int(l.lines),
                default_branch_new_lines_count: int(l.default_branch_lines),
                other_branches_new_lines_count: int(l.lines.saturating_sub(l.default_branch_lines)),
            }),
            (None, Some(r)) if r.indexed => Some(TextSearchIndexStatus {
                updated_at: r.last_indexed_at.unwrap_or(r.updated_at),
                content_byte_size: BigInt(r.content_bytes),
                content_files_count: to_int(r.docs),
                index_byte_size: to_int(r.index_bytes),
                index_shards_count: r.shards,
                new_lines_count: to_int(r.lines),
                default_branch_new_lines_count: to_int(r.default_branch_lines),
                other_branches_new_lines_count: to_int(r.lines - r.default_branch_lines),
            }),
            _ => None,
        }
    }

    /// The refs configured for indexing (the default branch plus `search.index.branches`),
    /// whether each is indexed and whether the index is of its current commit.
    async fn refs(&self, ctx: &Context<'_>) -> Result<Vec<TextSearchIndexedRef>> {
        let h = RepoHandle::new(ctx, &self.repo)?;
        let indexed = self.indexed_branches().await;
        // The refs as they are now; empty while the repository is not cloned.
        let listed = h.git.list_refs(true, true, "").await.ok();
        let mut names = vec![HEAD.to_owned()];
        for b in self.extra.iter().chain(indexed.iter().map(|(n, _)| n)) {
            if !names.contains(b) {
                names.push(b.clone());
            }
        }
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let current = listed.as_ref().and_then(|l| {
                let full = if name == HEAD {
                    l.head_ref.clone()
                } else {
                    name.clone()
                };
                l.refs
                    .iter()
                    .find(|r| r.name == full || r.short_name == full)
                    .cloned()
            });
            let indexed_commit = indexed
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, c)| c.clone());
            let r = match current {
                Some(r) => GitRefNode::from_ref(h.clone(), r),
                // Not a ref now (deleted, a commit, or the repository is not cloned).
                None => GitRefNode::branch(h.clone(), &name, ""),
            };
            let current_oid = listed.as_ref().and_then(|l| {
                let full = if name == HEAD { &l.head_ref } else { &name };
                l.refs
                    .iter()
                    .find(|r| &r.name == full || &r.short_name == full)
                    .map(|r| r.commit_oid.clone())
            });
            out.push(TextSearchIndexedRef {
                name,
                current: indexed_commit.is_some() && indexed_commit == current_oid,
                indexed_commit: indexed_commit.map(|c| GitObjectNode::new(h.clone(), c)),
                r,
            });
        }
        Ok(out)
    }

    /// The repo node that holds the repository's index.
    async fn host(&self) -> Option<IndexserverHost> {
        self.host.clone().map(|name| IndexserverHost { name })
    }

    /// The error of the last build attempt, if it failed (cleared by the next success).
    async fn last_error(&self) -> Option<&str> {
        self.row.as_ref()?.last_error.as_deref()
    }

    /// When the last build attempt failed.
    async fn last_error_at(&self) -> Option<DateTime<Utc>> {
        self.row.as_ref()?.last_error_at
    }

    /// Whether a build of the repository is queued on its repo node.
    async fn queued(&self) -> bool {
        self.live_status().await.is_ok_and(|l| l.queued)
    }

    /// When the index last changed, as the repo node last reported it (cheap, without asking the
    /// node).
    async fn last_indexed_at(&self) -> Option<DateTime<Utc>> {
        self.row.as_ref().filter(|r| r.indexed)?.last_indexed_at
    }

    /// The index files of the repository on its repo node (empty when not indexed or the
    /// node could not be asked).
    async fn shards(&self) -> Vec<TextSearchIndexShard> {
        self.live_status()
            .await
            .ok()
            .map(|l| {
                l.shards
                    .iter()
                    .map(|s| TextSearchIndexShard {
                        kind: s.kind.clone(),
                        file: s.file.clone(),
                        byte_size: big(s.size_bytes),
                        files_count: int(s.docs),
                        repositories_count: i32::try_from(s.repos).unwrap_or(i32::MAX),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Why the repo node could not be asked for the live status, if it could not.
    async fn node_error(&self) -> Option<&str> {
        self.live_status().await.err()
    }
}

/// `RepositoryTextSearchIndexStatus`.
#[derive(SimpleObject)]
#[graphql(name = "RepositoryTextSearchIndexStatus")]
pub(crate) struct TextSearchIndexStatus {
    /// When the index was last updated.
    updated_at: DateTime<Utc>,
    /// Bytes of the text files indexed.
    content_byte_size: BigInt,
    /// Files indexed (over all indexed branches, each version once).
    content_files_count: i32,
    /// Bytes of the index on disk.
    index_byte_size: i32,
    /// Index files.
    index_shards_count: i32,
    /// Lines in the index.
    new_lines_count: i32,
    /// Lines of the files on the default branch.
    default_branch_new_lines_count: i32,
    /// Lines of the files only on other branches.
    other_branches_new_lines_count: i32,
}

/// `repositoryIndexserverHost` (lower-case).
#[derive(SimpleObject)]
#[graphql(name = "repositoryIndexserverHost")]
pub(crate) struct IndexserverHost {
    /// The repo node's address.
    name: String,
}

/// One index file of a repository.
#[derive(SimpleObject)]
#[graphql(name = "RepositoryTextSearchIndexShard")]
pub(crate) struct TextSearchIndexShard {
    /// `base` (the repository's own shard), `compound` (shared with other small
    /// repositories) or `delta` (changes since the base was built).
    kind: String,
    /// File name on the repo node.
    file: String,
    /// Bytes on disk; for a compound shard, the whole file.
    byte_size: BigInt,
    /// Files of this repository in the shard.
    files_count: i32,
    /// Repositories in the shard.
    repositories_count: i32,
}

/// `RepositoryTextSearchIndexedRef`.
pub(crate) struct TextSearchIndexedRef {
    name: String,
    r: GitRefNode,
    current: bool,
    indexed_commit: Option<GitObjectNode>,
}

#[Object(name = "RepositoryTextSearchIndexedRef")]
impl TextSearchIndexedRef {
    /// The ref configured for indexing; its target is the current one, not the indexed one.
    #[graphql(name = "ref")]
    async fn git_ref(&self) -> &GitRefNode {
        &self.r
    }

    /// Whether the index holds this ref.
    async fn indexed(&self) -> bool {
        self.indexed_commit.is_some()
    }

    /// Whether the index is of the ref's current commit (false: the index is stale).
    async fn current(&self) -> bool {
        self.current
    }

    /// The commit indexed; null when the ref is not indexed.
    async fn indexed_commit(&self) -> Option<&GitObjectNode> {
        self.indexed_commit.as_ref()
    }

    /// The name the ref is indexed under (`HEAD` for the default branch).
    async fn index_name(&self) -> &str {
        &self.name
    }
}

// ------------------------------------------------------------------ stats and nodes

/// `RepositoryStats`.
pub(crate) struct RepositoryStatsNode(sourcegull_db::RepositoryStats);

#[Object(name = "RepositoryStats")]
impl RepositoryStatsNode {
    /// Bytes of the repositories on the repo nodes, as last measured.
    async fn git_dir_bytes(&self) -> BigInt {
        BigInt(self.0.git_dir_bytes)
    }

    /// Lines in the text search index.
    async fn indexed_lines_count(&self) -> BigInt {
        BigInt(self.0.indexed_lines)
    }

    /// Repositories, without deleted or blocked ones.
    async fn total(&self) -> i32 {
        to_int(self.0.total)
    }

    /// Cloned repositories.
    async fn cloned(&self) -> i32 {
        to_int(self.0.cloned)
    }

    /// Repositories being cloned.
    async fn cloning(&self) -> i32 {
        to_int(self.0.cloning)
    }

    /// Repositories not cloned yet.
    async fn not_cloned(&self) -> i32 {
        to_int(self.0.not_cloned)
    }

    /// Repositories waiting for their first clone with no failed attempt.
    async fn queued(&self) -> i32 {
        to_int(self.0.queued)
    }

    /// Cloned repositories without a text search index yet.
    async fn not_indexed(&self) -> i32 {
        to_int(self.0.not_indexed)
    }

    /// Repositories whose code the code host token may not download (GitLab: below
    /// Reporter on a private project). Cloned empty and not failures.
    async fn no_code_access(&self) -> i32 {
        to_int(self.0.no_code_access)
    }

    /// Repositories whose last clone or fetch failed.
    async fn failed_fetch(&self) -> i32 {
        to_int(self.0.failed_fetch)
    }

    /// Repositories with a text search index.
    async fn indexed(&self) -> i32 {
        to_int(self.0.indexed)
    }

    /// Repositories their repo node found corrupt.
    async fn corrupted(&self) -> i32 {
        to_int(self.0.corrupted)
    }

    /// Repositories with embeddings: always 0 (embeddings are not supported).
    async fn embedded(&self) -> i32 {
        0
    }
}

/// `GitserverInstance`: a repo node.
#[derive(SimpleObject)]
#[graphql(name = "GitserverInstance")]
pub(crate) struct GitserverInstanceNode {
    /// An opaque ID.
    id: ID,
    /// The node's address.
    address: String,
    /// Bytes free on the node's data volume (0 when the node could not be asked).
    free_disk_space_bytes: BigInt,
    /// Size of the node's data volume (0 when the node could not be asked).
    total_disk_space_bytes: BigInt,
    /// Whether the node is in the configured node list (a node that left
    /// it may still hold repositories until they are moved).
    configured: bool,
    /// Why the node could not be asked for its disk usage, if it could not.
    error: Option<String>,
    /// Free bytes below which the node pauses new clones (null when the
    /// node could not be asked).
    clone_pause_threshold_bytes: Option<BigInt>,
    /// Whether free space is below that threshold, so clones are paused
    /// (false when the node could not be asked).
    clones_paused: bool,
    /// Repositories the node holds (cloned or cloning).
    repositories_count: i32,
    /// Repositories assigned to the node.
    assigned_repositories_count: i32,
    /// Bytes of those repositories, as last measured.
    repositories_disk_bytes: BigInt,
    /// Bare repositories on the node's disk now (null when the node could not be asked).
    repositories_on_disk: Option<i32>,
    /// Repositories the node has indexed.
    indexed_repositories_count: i32,
    /// Bytes of the node's index directory (shards); null when the node could not be
    /// asked.
    index_disk_bytes: Option<BigInt>,
}

/// `GitserverInstanceConnection`.
#[derive(SimpleObject)]
#[graphql(name = "GitserverInstanceConnection")]
pub(crate) struct GitserverInstanceConnection {
    nodes: Vec<GitserverInstanceNode>,
    total_count: i32,
    page_info: PageInfo,
}

async fn gitserver_instances(ctx: &Context<'_>) -> Result<GitserverInstanceConnection> {
    let svc = services(ctx)?;
    let usage: BTreeMap<String, NodeUsage> = svc
        .db
        .search_index()
        .node_usage()
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|u| (u.node.clone(), u))
        .collect();
    let configured: Vec<String> = svc
        .repo_clients
        .router()
        .nodes()
        .map(str::to_owned)
        .collect();
    let mut addrs = configured.clone();
    addrs.extend(usage.keys().filter(|n| !configured.contains(n)).cloned());
    let infos = futures_join(addrs.iter().map(|addr| {
        let clients = svc.repo_clients.clone();
        let addr = addr.clone();
        async move {
            match clients.for_node(&addr) {
                Ok(mut c) => call(c.disk_info(pb::DiskInfoRequest {})).await,
                Err(e) => Err(e.to_string()),
            }
        }
    }))
    .await;
    let nodes: Vec<GitserverInstanceNode> = addrs
        .into_iter()
        .zip(infos)
        .map(|(addr, info)| {
            let u = usage.get(&addr).cloned().unwrap_or_default();
            let (disk, error) = match info {
                Ok(d) => (Some(d), None),
                Err(e) => (None, Some(e)),
            };
            GitserverInstanceNode {
                id: ID(marshal_string_id("GitserverInstance", &addr)),
                configured: configured.contains(&addr),
                free_disk_space_bytes: big(disk.as_ref().map_or(0, |d| d.free_bytes)),
                total_disk_space_bytes: big(disk.as_ref().map_or(0, |d| d.total_bytes)),
                repositories_on_disk: disk.as_ref().map(|d| int(d.repos_on_disk)),
                index_disk_bytes: disk.as_ref().map(|d| big(d.index_bytes)),
                clone_pause_threshold_bytes: disk.as_ref().map(|d| big(d.pause_clones_below_bytes)),
                clones_paused: disk.as_ref().is_some_and(|d| d.clones_paused),
                error,
                repositories_count: to_int(u.repos),
                assigned_repositories_count: to_int(u.assigned),
                repositories_disk_bytes: BigInt(u.repo_bytes),
                indexed_repositories_count: to_int(u.indexed_repos),
                address: addr,
            }
        })
        .collect();
    Ok(GitserverInstanceConnection {
        total_count: to_int(i64::try_from(nodes.len()).unwrap_or(i64::MAX)),
        nodes,
        page_info: PageInfo {
            has_next_page: false,
            end_cursor: None,
        },
    })
}

/// Runs the futures concurrently and returns their outputs in order.
async fn futures_join<T: Send + 'static>(
    futs: impl Iterator<Item = impl Future<Output = T> + Send + 'static>,
) -> Vec<T> {
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        // The tasks do not panic; a join error would be a runtime shutdown.
        out.push(h.await.expect("node call task"));
    }
    out
}

// ------------------------------------------------------------------ rebalancing

/// `RepoNodeHeartbeat`: a repo node's last heartbeat.
#[derive(SimpleObject)]
#[graphql(name = "RepoNodeHeartbeat")]
pub(crate) struct RepoNodeHeartbeatNode {
    /// The node's address.
    address: String,
    /// The node list the node runs with.
    node_list: Vec<String>,
    /// When the node process started.
    started_at: DateTime<Utc>,
    /// The node's last heartbeat.
    last_seen_at: DateTime<Utc>,
    /// Whether the heartbeat is recent.
    alive: bool,
    /// Whether the node is in the node list this web process runs with.
    configured: bool,
}

/// `RepoMove`: the move of a repository to another repo node.
pub(crate) struct RepoMoveNode(sourcegull_db::RepoMove);

#[Object(name = "RepoMove")]
impl RepoMoveNode {
    /// The repository's name.
    async fn repository_name(&self) -> &str {
        &self.0.repo_name
    }

    /// The repository's ID.
    async fn repository_id(&self) -> ID {
        ID(sourcegull_services::relay::marshal_id(
            crate::repos::REPOSITORY_KIND,
            i64::from(self.0.repo_id.get()),
        ))
    }

    /// The node the repository moves away from.
    async fn from_node(&self) -> &str {
        &self.0.from_node
    }

    /// The node the repository moves to.
    async fn to_node(&self) -> &str {
        &self.0.to_node
    }

    /// `copy` (from the old node) or `reclone` (the old node is gone; from the code host).
    async fn kind(&self) -> &str {
        &self.0.kind
    }

    /// `queued`, `copying`, `flipped` (switched over; the old copy is removed after a grace
    /// period), `done`, `failed` or `cancelled`.
    async fn state(&self) -> &str {
        &self.0.state
    }

    /// Attempts so far.
    async fn attempts(&self) -> i32 {
        self.0.attempts
    }

    /// Pack bytes received by the new node in the current attempt.
    async fn bytes_copied(&self) -> BigInt {
        BigInt(self.0.bytes_copied)
    }

    /// The error of the last failed attempt, or why the move was cancelled.
    async fn last_error(&self) -> Option<&str> {
        self.0.last_error.as_deref()
    }

    /// When a queued retry is due.
    async fn next_attempt_at(&self) -> Option<DateTime<Utc>> {
        self.0.next_attempt_at
    }

    /// When the move was planned.
    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the current attempt started.
    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.0.started_at
    }

    /// When the new node took over.
    async fn flipped_at(&self) -> Option<DateTime<Utc>> {
        self.0.flipped_at
    }

    /// When the move ended.
    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.0.finished_at
    }

    /// When the move last changed.
    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }
}

/// `RepoRebalanceStatus`: repository moves between repo nodes after nodes were added or
/// removed.
pub(crate) struct RepoRebalanceStatusNode {
    heartbeats: Vec<sourcegull_db::RepoNodeHeartbeat>,
    configured: Vec<String>,
    now: DateTime<Utc>,
    counts: sourcegull_db::MoveCounts,
}

#[Object(name = "RepoRebalanceStatus")]
impl RepoRebalanceStatusNode {
    /// Configured repo nodes that are down or run with another node list. Nothing is moved
    /// while any are (a rollout in progress).
    async fn waiting_for(&self) -> Vec<String> {
        let live = sourcegull_db::live_repo_nodes(
            &self.heartbeats,
            self.now,
            sourcegull_db::REPO_NODE_LIVE_WINDOW,
        );
        sourcegull_db::unsettled_repo_nodes(&self.configured, &self.heartbeats, &live)
    }

    /// Every repo node's last heartbeat.
    async fn nodes(&self) -> Vec<RepoNodeHeartbeatNode> {
        let live = sourcegull_db::live_repo_nodes(
            &self.heartbeats,
            self.now,
            sourcegull_db::REPO_NODE_LIVE_WINDOW,
        );
        self.heartbeats
            .iter()
            .map(|h| RepoNodeHeartbeatNode {
                address: h.addr.clone(),
                node_list: h.nodes.clone(),
                started_at: h.started_at,
                last_seen_at: h.seen_at,
                alive: live.contains(&h.addr),
                configured: self.configured.contains(&h.addr),
            })
            .collect()
    }

    /// Moves waiting for their new node.
    async fn queued(&self) -> i32 {
        to_int(self.counts.queued)
    }

    /// Moves being copied.
    async fn copying(&self) -> i32 {
        to_int(self.counts.copying)
    }

    /// Moves switched over whose old copy is not removed yet.
    async fn flipped(&self) -> i32 {
        to_int(self.counts.flipped)
    }

    /// Moves that gave up (the repositories stay on their old node).
    async fn failed(&self) -> i32 {
        to_int(self.counts.failed)
    }

    /// Finished moves kept for a week.
    async fn done(&self) -> i32 {
        to_int(self.counts.done)
    }

    /// Cancelled moves kept for a week.
    async fn cancelled(&self) -> i32 {
        to_int(self.counts.cancelled)
    }

    /// Moves in `states` (all when empty or omitted), the most recently changed first.
    async fn moves(
        &self,
        ctx: &Context<'_>,
        states: Option<Vec<String>>,
        #[graphql(default = 50)] first: i32,
    ) -> Result<Vec<RepoMoveNode>> {
        let states = states.unwrap_or_default();
        if let Some(bad) = states
            .iter()
            .find(|s| !sourcegull_db::move_state::ALL.contains(&s.as_str()))
        {
            return Err(error(
                codes::INVALID_INPUT,
                format!("Unknown move state {bad:?}."),
            ));
        }
        let first = i64::from(first.clamp(1, 500));
        let moves = services(ctx)?
            .db
            .repo_moves()
            .list(&states, first)
            .await
            .map_err(db_error)?;
        Ok(moves.into_iter().map(RepoMoveNode).collect())
    }
}

// ------------------------------------------------------------------ roots

/// Site admin repository queries.
#[derive(Default)]
pub struct ReposAdminQuery;

#[Object]
impl ReposAdminQuery {
    /// Repository counts of the instance (site admins only).
    async fn repository_stats(&self, ctx: &Context<'_>) -> Result<RepositoryStatsNode> {
        require_site_admin(ctx)?;
        let stats = services(ctx)?
            .db
            .search_index()
            .repository_stats()
            .await
            .map_err(db_error)?;
        Ok(RepositoryStatsNode(stats))
    }

    /// The repo nodes with their disk usage (site admins only).
    async fn gitservers(&self, ctx: &Context<'_>) -> Result<GitserverInstanceConnection> {
        require_site_admin(ctx)?;
        gitserver_instances(ctx).await
    }

    /// Repository moves between repo nodes after nodes were added or removed (site admins
    /// only).
    async fn repo_rebalance(&self, ctx: &Context<'_>) -> Result<RepoRebalanceStatusNode> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let store = svc.db.repo_moves();
        Ok(RepoRebalanceStatusNode {
            heartbeats: store.heartbeats().await.map_err(db_error)?,
            now: store.now().await.map_err(db_error)?,
            counts: store.counts().await.map_err(db_error)?,
            configured: svc
                .repo_clients
                .router()
                .nodes()
                .map(str::to_owned)
                .collect(),
        })
    }
}

/// Site admin repository mutations.
#[derive(Default)]
pub struct ReposAdminMutation;

/// The live repository `id` names, or `NOT_FOUND`.
async fn live_repo(ctx: &Context<'_>, id: &ID) -> Result<Repo> {
    let id: RepoId = parse_repository_id(id)?;
    // Site admins only.
    #[allow(clippy::disallowed_methods)]
    services(ctx)?
        .db
        .repos()
        .get(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| error(codes::NOT_FOUND, "Repository not found."))
}

#[Object]
impl ReposAdminMutation {
    /// Clones the repository again from its code host, replacing the copy on its repo node
    /// once the new clone is complete (site admins only). A repository not cloned yet is
    /// cloned.
    async fn reclone_repository(&self, ctx: &Context<'_>, repo: ID) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let repo = live_repo(ctx, &repo).await?;
        services(ctx)?
            .db
            .repo_update_jobs()
            .enqueue(
                repo.id,
                RepoUpdatePriority::IMMEDIATE,
                RepoUpdateReason::Reclone,
            )
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Queues the failed repository moves between repo nodes again (site admins only).
    /// Returns how many.
    async fn retry_failed_repo_moves(&self, ctx: &Context<'_>) -> Result<i32> {
        require_site_admin(ctx)?;
        let n = services(ctx)?
            .db
            .repo_moves()
            .retry_failed()
            .await
            .map_err(db_error)?;
        Ok(i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// Rebuilds the repository's text search index from scratch on its repo node (site
    /// admins only). Indexing runs on its own after every fetch, so this is rarely needed.
    async fn reindex_repository(&self, ctx: &Context<'_>, repository: ID) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        let repo = live_repo(ctx, &repository).await?;
        let svc = services(ctx)?;
        let mut client = svc
            .repo_clients
            .search_for_repo(repo.id)
            .map_err(|e| error(codes::UNAVAILABLE, e.to_string()))?;
        call(client.reindex(pb::ReindexRequest {
            repo: Some(pb::RepoRef {
                id: repo.id.get(),
                name: repo.name.clone(),
            }),
        }))
        .await
        .map_err(|e| error(codes::UNAVAILABLE, format!("The repo node refused: {e}")))?;
        Ok(EmptyResponse)
    }
}
