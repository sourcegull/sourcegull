//! `search_index_repos` store: the text search index state of each repo as its repo node last
//! reported it, plus the aggregates of the site admin's repository pages:
//! instance-wide repository counts and per repo node usage.
//!
//! The repo node's indexer writes a row after each build, removal or failed attempt; readers
//! are the admin GraphQL resolvers. Writes apply only while the repo is assigned to the writing
//! node (or to none): see `OWNED_BY_NODE`.

use chrono::{DateTime, Utc};
use sourcegull_types::RepoId;
use sqlx::PgPool;

use crate::Result;

/// A branch and the commit it is indexed at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexedBranchRow {
    pub name: String,
    pub commit: String,
}

/// What a finished build left in the index, for [`SearchIndexStore::record_indexed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexedState {
    pub branches: Vec<IndexedBranchRow>,
    pub docs: i64,
    pub content_bytes: i64,
    pub index_bytes: i64,
    pub lines: i64,
    pub default_branch_lines: i64,
    pub shards: i32,
    /// When the index last changed; `None` keeps the stored time (or uses now for a new row).
    pub indexed_at: Option<DateTime<Utc>>,
}

/// A repo's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchIndexRepo {
    pub repo_id: RepoId,
    pub node: String,
    pub indexed: bool,
    pub branches: Vec<IndexedBranchRow>,
    pub last_indexed_at: Option<DateTime<Utc>>,
    pub docs: i64,
    pub content_bytes: i64,
    pub index_bytes: i64,
    pub lines: i64,
    pub default_branch_lines: i64,
    pub shards: i32,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Row {
    repo_id: RepoId,
    node: String,
    index_status: String,
    branches: sqlx::types::Json<Vec<IndexedBranchRow>>,
    last_indexed_at: Option<DateTime<Utc>>,
    docs: i64,
    content_bytes: i64,
    index_bytes: i64,
    lines: i64,
    default_branch_lines: i64,
    shards: i32,
    last_error: Option<String>,
    last_error_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

impl From<Row> for SearchIndexRepo {
    fn from(r: Row) -> Self {
        Self {
            repo_id: r.repo_id,
            node: r.node,
            indexed: r.index_status == "indexed",
            branches: r.branches.0,
            last_indexed_at: r.last_indexed_at,
            docs: r.docs,
            content_bytes: r.content_bytes,
            index_bytes: r.index_bytes,
            lines: r.lines,
            default_branch_lines: r.default_branch_lines,
            shards: r.shards,
            last_error: r.last_error,
            last_error_at: r.last_error_at,
            updated_at: r.updated_at,
        }
    }
}

/// The `last_error` a repo node records for a clone it skipped because it is low on disk
/// space; `statusMessages` looks for it.
pub const CLONES_PAUSED_ERROR: &str = "clones are paused: the repo node is low on disk space";

/// Instance-wide repository counts (`repositoryStats`). Soft-deleted and blocked repos are
/// left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct RepositoryStats {
    pub total: i64,
    pub cloned: i64,
    pub cloning: i64,
    pub not_cloned: i64,
    pub failed_fetch: i64,
    pub corrupted: i64,
    pub indexed: i64,
    /// Cloned repos without a text search index yet.
    pub not_indexed: i64,
    /// Repos waiting for their first clone with no failed attempt (`not_cloned`, no error).
    pub queued: i64,
    /// Cloned-empty repos whose code the code host token may not download
    /// (`remote_unavailable = 'no_code_access'`); not failures.
    pub no_code_access: i64,
    /// Sum of the repos' sizes on disk as last measured by their nodes.
    pub git_dir_bytes: i64,
    /// Lines in the index.
    pub indexed_lines: i64,
}

/// What `gitserver_repos` and `search_index_repos` say about one repo node.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct NodeUsage {
    pub node: String,
    /// Repos the node reports holding (cloned or cloning).
    pub repos: i64,
    /// Their sizes on disk, as last measured.
    pub repo_bytes: i64,
    /// Repos the node has indexed, and the bytes of their shards.
    pub indexed_repos: i64,
    pub index_bytes: i64,
    /// Repos assigned to the node (`repo.shard`).
    pub assigned: i64,
}

/// Reads and writes `search_index_repos`.
#[derive(Debug, Clone)]
pub struct SearchIndexStore {
    pool: PgPool,
}

/// The condition of every write here and of the repo node's clone records in `gitserver_repos`:
/// the repo `$1` exists and is assigned to node `$2` or to none.
/// `FOR SHARE` makes a concurrent reassignment (a move's flip updates `repo.shard` and then
/// both tables in one transaction) either wait for the write or be seen by it, so a write
/// that ends after the flip leaves the new owner's rows alone.
pub(crate) const OWNED_BY_NODE: &str = "EXISTS (SELECT 1 FROM repo WHERE id = $1 \
     AND (shard IS NULL OR shard = $2) FOR SHARE)";

const COLUMNS: &str = "repo_id, node, index_status, branches, last_indexed_at, docs, \
     content_bytes, index_bytes, lines, default_branch_lines, shards, last_error, last_error_at, \
     updated_at";

impl SearchIndexStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The row of a repo, if its node ever reported one.
    pub async fn get(&self, repo: RepoId) -> Result<Option<SearchIndexRepo>> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM search_index_repos WHERE repo_id = $1"
        ))
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Rows of several repos (in no particular order; missing ones are left out).
    pub async fn get_many(&self, repos: &[RepoId]) -> Result<Vec<SearchIndexRepo>> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM search_index_repos WHERE repo_id = ANY($1)"
        ))
        .bind(repos)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Records that `node` holds an index of `repo` as described by `state`, clearing the
    /// error. Returns `false`, writing nothing, when the repo was deleted or is assigned to
    /// another node.
    pub async fn record_indexed(
        &self,
        repo: RepoId,
        node: &str,
        state: &IndexedState,
    ) -> Result<bool> {
        let written = sqlx::query(&format!(
            "INSERT INTO search_index_repos AS s (repo_id, node, index_status, branches,
                 last_indexed_at, docs, content_bytes, index_bytes, lines, default_branch_lines,
                 shards)
             SELECT $1, $2, 'indexed', $3, coalesce($4, now()), $5, $6, $7, $8, $9, $10
             WHERE {OWNED_BY_NODE}
             ON CONFLICT (repo_id) DO UPDATE SET node = EXCLUDED.node, index_status = 'indexed',
                 branches = EXCLUDED.branches,
                 last_indexed_at = CASE WHEN $4::timestamptz IS NULL
                     THEN coalesce(s.last_indexed_at, now()) ELSE $4 END,
                 docs = EXCLUDED.docs, content_bytes = EXCLUDED.content_bytes,
                 index_bytes = EXCLUDED.index_bytes, lines = EXCLUDED.lines,
                 default_branch_lines = EXCLUDED.default_branch_lines, shards = EXCLUDED.shards,
                 last_error = NULL, last_error_at = NULL, updated_at = now()"
        ))
        .bind(repo)
        .bind(node)
        .bind(sqlx::types::Json(&state.branches))
        .bind(state.indexed_at)
        .bind(state.docs)
        .bind(state.content_bytes)
        .bind(state.index_bytes)
        .bind(state.lines)
        .bind(state.default_branch_lines)
        .bind(state.shards)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Records that `node` holds no index of `repo` (nothing to index, or the repo left the
    /// node). The last error is kept. Returns `false`, writing nothing, when the repo was
    /// deleted or is assigned to another node.
    pub async fn record_not_indexed(&self, repo: RepoId, node: &str) -> Result<bool> {
        let written = sqlx::query(&format!(
            "INSERT INTO search_index_repos (repo_id, node)
             SELECT $1, $2 WHERE {OWNED_BY_NODE}
             ON CONFLICT (repo_id) DO UPDATE SET node = EXCLUDED.node,
                 index_status = 'not_indexed', branches = '[]'::jsonb, docs = 0,
                 content_bytes = 0, index_bytes = 0, lines = 0, default_branch_lines = 0,
                 shards = 0, updated_at = now()"
        ))
        .bind(repo)
        .bind(node)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Records a failed build of `repo` on `node`. What the index holds is unchanged (a failed
    /// build leaves the previous shards in place). Returns `false`, writing nothing, when the
    /// repo was deleted or is assigned to another node.
    pub async fn record_error(&self, repo: RepoId, node: &str, error: &str) -> Result<bool> {
        let written = sqlx::query(&format!(
            "INSERT INTO search_index_repos (repo_id, node, last_error, last_error_at)
             SELECT $1, $2, $3, now() WHERE {OWNED_BY_NODE}
             ON CONFLICT (repo_id) DO UPDATE SET node = EXCLUDED.node, last_error = $3,
                 last_error_at = now(), updated_at = now()"
        ))
        .bind(repo)
        .bind(node)
        .bind(error)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(written > 0)
    }

    /// Instance-wide repository counts.
    pub async fn repository_stats(&self) -> Result<RepositoryStats> {
        Ok(sqlx::query_as(
            "SELECT count(*) AS total,
                    count(*) FILTER (WHERE gr.clone_status = 'cloned') AS cloned,
                    count(*) FILTER (WHERE gr.clone_status = 'cloning') AS cloning,
                    count(*) FILTER (WHERE coalesce(gr.clone_status, 'not_cloned') = 'not_cloned')
                        AS not_cloned,
                    count(*) FILTER (WHERE gr.last_error IS NOT NULL) AS failed_fetch,
                    count(*) FILTER (WHERE gr.corrupted_at IS NOT NULL) AS corrupted,
                    count(*) FILTER (WHERE s.index_status = 'indexed') AS indexed,
                    count(*) FILTER (WHERE gr.clone_status = 'cloned'
                        AND s.index_status IS DISTINCT FROM 'indexed'
                        -- empty repos (no commits, or no repository the node can fetch:
                        -- none yet, disabled, no code access) have nothing to index
                        AND gr.remote_unavailable IS NULL AND gr.head_oid IS NOT NULL)
                        AS not_indexed,
                    count(*) FILTER (WHERE coalesce(gr.clone_status, 'not_cloned') = 'not_cloned'
                        AND gr.last_error IS NULL) AS queued,
                    count(*) FILTER (WHERE gr.remote_unavailable = 'no_code_access')
                        AS no_code_access,
                    coalesce(sum(gr.repo_size_bytes), 0)::bigint AS git_dir_bytes,
                    coalesce(sum(s.lines) FILTER (WHERE s.index_status = 'indexed'), 0)::bigint
                        AS indexed_lines
             FROM repo r
             LEFT JOIN gitserver_repos gr ON gr.repo_id = r.id
             LEFT JOIN search_index_repos s ON s.repo_id = r.id
             WHERE r.deleted_at IS NULL AND r.blocked IS NULL",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    /// Repos waiting for a clone because their repo node paused clones for lack of disk
    /// space ([`CLONES_PAUSED_ERROR`]), per node (the repo's assigned node).
    pub async fn clones_paused(&self) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(
            "SELECT coalesce(r.shard, gr.node, '') AS node, count(*)
             FROM repo r JOIN gitserver_repos gr ON gr.repo_id = r.id
             WHERE r.deleted_at IS NULL AND r.blocked IS NULL
               AND coalesce(gr.clone_status, 'not_cloned') = 'not_cloned'
               AND starts_with(gr.last_error, $1)
             GROUP BY 1 ORDER BY 1",
        )
        .bind(CLONES_PAUSED_ERROR)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Per repo node: repos held and assigned, their sizes, and the index. Nodes come from
    /// `gitserver_repos.node`, `repo.shard` and `search_index_repos.node`, so a node that left
    /// the configured list but still owns repos shows up too.
    pub async fn node_usage(&self) -> Result<Vec<NodeUsage>> {
        Ok(sqlx::query_as(
            "WITH held AS (
                 SELECT gr.node, count(*) AS repos,
                        coalesce(sum(gr.repo_size_bytes), 0)::bigint AS repo_bytes
                 FROM gitserver_repos gr JOIN repo r ON r.id = gr.repo_id
                 WHERE r.deleted_at IS NULL AND gr.node IS NOT NULL
                   AND gr.clone_status IN ('cloned', 'cloning')
                 GROUP BY gr.node),
             indexed AS (
                 SELECT s.node, count(*) AS indexed_repos,
                        coalesce(sum(s.index_bytes), 0)::bigint AS index_bytes
                 FROM search_index_repos s JOIN repo r ON r.id = s.repo_id
                 WHERE r.deleted_at IS NULL AND s.index_status = 'indexed'
                 GROUP BY s.node),
             assigned AS (
                 SELECT shard AS node, count(*) AS assigned FROM repo
                 WHERE deleted_at IS NULL AND shard IS NOT NULL GROUP BY shard),
             nodes AS (
                 SELECT node FROM held UNION SELECT node FROM indexed
                 UNION SELECT node FROM assigned)
             SELECT n.node, coalesce(h.repos, 0) AS repos, coalesce(h.repo_bytes, 0) AS repo_bytes,
                    coalesce(i.indexed_repos, 0) AS indexed_repos,
                    coalesce(i.index_bytes, 0) AS index_bytes,
                    coalesce(a.assigned, 0) AS assigned
             FROM nodes n
             LEFT JOIN held h ON h.node = n.node
             LEFT JOIN indexed i ON i.node = n.node
             LEFT JOIN assigned a ON a.node = n.node
             ORDER BY n.node",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
