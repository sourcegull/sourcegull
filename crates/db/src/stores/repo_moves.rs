//! `repo_nodes` and `repo_moves`: repo node heartbeats and the moves of repos between repo
//! nodes when nodes are added or removed (rebalancing).
//!
//! The planner (one repo node at a time, under a transaction-scoped advisory lock) writes
//! `queued` moves through [`Planning`]; the new owner claims them, copies the repo, and flips
//! the assignment with [`RepoMoveStore::flip`]; the old owner removes its copy after a grace
//! period and marks the move `done`. Site admins read the state on the repo nodes page.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_types::RepoId;
use sqlx::{PgPool, Postgres, Transaction};

use crate::Result;
use crate::notify::{channels, notify};

/// How a repo moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveKind {
    /// Copy from the old owner, verify, flip, then remove the old copy.
    Copy,
    /// The old owner is gone: flip and clone again from the code host.
    Reclone,
}

impl MoveKind {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            MoveKind::Copy => "copy",
            MoveKind::Reclone => "reclone",
        }
    }
}

/// States of a move, as stored.
pub mod move_state {
    /// Waiting for the new owner.
    pub const QUEUED: &str = "queued";
    /// The new owner is copying.
    pub const COPYING: &str = "copying";
    /// The assignment points at the new owner; the old copy is still on the old owner.
    pub const FLIPPED: &str = "flipped";
    /// Finished.
    pub const DONE: &str = "done";
    /// Gave up after the last attempt (a site admin can retry).
    pub const FAILED: &str = "failed";
    /// No longer wanted (the hash owner changed again, or the repo was deleted).
    pub const CANCELLED: &str = "cancelled";
    /// Every state, in lifecycle order.
    pub const ALL: &[&str] = &[QUEUED, COPYING, FLIPPED, DONE, FAILED, CANCELLED];
}

/// A move, with the repo's name.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoMove {
    pub repo_id: RepoId,
    pub repo_name: String,
    pub from_node: String,
    pub to_node: String,
    pub kind: String,
    pub state: String,
    pub attempts: i32,
    pub bytes_copied: i64,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub flipped_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// A move for the planner to queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRepoMove {
    pub repo_id: RepoId,
    pub from_node: String,
    pub to_node: String,
    pub kind: MoveKind,
}

/// A move that is not finished, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OpenRepoMove {
    pub repo_id: RepoId,
    pub from_node: String,
    pub to_node: String,
    pub state: String,
}

/// A repo node's last heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoNodeHeartbeat {
    pub addr: String,
    /// The node list the node runs with, sorted.
    pub nodes: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub seen_at: DateTime<Utc>,
}

/// Moves per state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveCounts {
    pub queued: i64,
    pub copying: i64,
    pub flipped: i64,
    pub done: i64,
    pub failed: i64,
    pub cancelled: i64,
}

const COLUMNS: &str = "m.repo_id, r.name AS repo_name, m.from_node, m.to_node, m.kind, m.state, \
     m.attempts, m.bytes_copied, m.last_error, m.next_attempt_at, m.created_at, m.started_at, \
     m.flipped_at, m.finished_at, m.updated_at";

/// The advisory lock key of the planner.
const PLANNER_LOCK: i64 = 0x5eec_0000_0001;

/// Reads and writes `repo_nodes` and `repo_moves`.
#[derive(Debug, Clone)]
pub struct RepoMoveStore {
    pool: PgPool,
}

impl RepoMoveStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ------------------------------------------------------------------ heartbeats

    /// Records that node `addr`, running with node list `nodes` since `started_at`, is alive.
    pub async fn heartbeat(
        &self,
        addr: &str,
        nodes: &[String],
        started_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut nodes = nodes.to_vec();
        nodes.sort();
        nodes.dedup();
        sqlx::query(
            "INSERT INTO repo_nodes (addr, nodes, started_at, seen_at) VALUES ($1, $2, $3, now())
             ON CONFLICT (addr) DO UPDATE
             SET nodes = EXCLUDED.nodes, started_at = EXCLUDED.started_at, seen_at = now()",
        )
        .bind(addr)
        .bind(&nodes)
        .bind(started_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every node's last heartbeat, by address.
    pub async fn heartbeats(&self) -> Result<Vec<RepoNodeHeartbeat>> {
        Ok(
            sqlx::query_as("SELECT addr, nodes, started_at, seen_at FROM repo_nodes ORDER BY addr")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// The database's clock (heartbeats are compared with it, not with the caller's).
    pub async fn now(&self) -> Result<DateTime<Utc>> {
        Ok(sqlx::query_scalar("SELECT now()")
            .fetch_one(&self.pool)
            .await?)
    }

    /// Forgets nodes not seen since `before` (nodes that left long ago).
    pub async fn forget_nodes(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM repo_nodes WHERE seen_at < $1")
            .bind(before)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    // ------------------------------------------------------------------ planner

    /// Starts a planning pass if no other node is planning: a transaction holding the
    /// planner's advisory lock until it ends. `None` when another node holds it.
    pub async fn begin_planning(&self) -> Result<Option<Planning>> {
        let mut tx = self.pool.begin().await?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(PLANNER_LOCK)
            .fetch_one(&mut *tx)
            .await?;
        Ok(locked.then_some(Planning { tx }))
    }

    // ------------------------------------------------------------------ new owner

    /// Claims up to `limit` due queued moves to node `to`: they become `copying`, with one
    /// more attempt.
    pub async fn claim(&self, to: &str, limit: i64) -> Result<Vec<RepoMove>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_as(&format!(
            "WITH c AS (
                 UPDATE repo_moves SET state = 'copying', attempts = attempts + 1,
                        bytes_copied = 0, started_at = now(), updated_at = now()
                 WHERE repo_id IN (
                     SELECT repo_id FROM repo_moves
                     WHERE to_node = $1 AND state = 'queued'
                       AND (next_attempt_at IS NULL OR next_attempt_at <= now())
                     ORDER BY created_at, repo_id
                     LIMIT $2
                     FOR UPDATE SKIP LOCKED)
                 RETURNING *)
             SELECT {COLUMNS} FROM c m JOIN repo r ON r.id = m.repo_id
             ORDER BY m.created_at, m.repo_id"
        ))
        .bind(to)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Puts node `to`'s `copying` moves back in the queue (after a restart, nothing of them
    /// is running). Returns how many.
    pub async fn release(&self, to: &str) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE repo_moves SET state = 'queued', updated_at = now()
             WHERE to_node = $1 AND state = 'copying'",
        )
        .bind(to)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Records copy progress; returns whether the move is still `copying` (the planner may
    /// have cancelled it).
    pub async fn progress(&self, repo: RepoId, to: &str, bytes: i64) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE repo_moves SET bytes_copied = $3, updated_at = now()
             WHERE repo_id = $1 AND to_node = $2 AND state = 'copying'",
        )
        .bind(repo)
        .bind(to)
        .bind(bytes)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Records a failed attempt: back to `queued` until `retry_at`, or `failed` when `None`.
    pub async fn fail(
        &self,
        repo: RepoId,
        to: &str,
        error: &str,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE repo_moves
             SET state = CASE WHEN $4::timestamptz IS NULL THEN 'failed' ELSE 'queued' END,
                 last_error = $3, next_attempt_at = $4, updated_at = now(),
                 finished_at = CASE WHEN $4::timestamptz IS NULL THEN now() END
             WHERE repo_id = $1 AND to_node = $2 AND state = 'copying'",
        )
        .bind(repo)
        .bind(to)
        .bind(error)
        .bind(retry_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cancels the move of `repo` to `to` if it is not flipped yet, with the reason.
    pub async fn cancel(&self, repo: RepoId, to: &str, reason: &str) -> Result<()> {
        sqlx::query(
            "UPDATE repo_moves SET state = 'cancelled', last_error = $3, updated_at = now(),
                    finished_at = now()
             WHERE repo_id = $1 AND to_node = $2 AND state IN ('queued', 'copying', 'failed')",
        )
        .bind(repo)
        .bind(to)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Flips a copied repo to its new owner `to` in one transaction: the move becomes
    /// `flipped`, `repo.shard` moves from `from` to `to`, the repo node rows follow, and
    /// `NOTIFY repo_shards` makes every routing table reload the repo. Returns `false` (and changes
    /// nothing but cancelling the move) when the move is no longer `copying` or the repo is
    /// no longer assigned to `from`.
    pub async fn flip(
        &self,
        repo: RepoId,
        from: &str,
        to: &str,
        size_bytes: Option<i64>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let moved = sqlx::query(
            "UPDATE repo_moves SET state = 'flipped', flipped_at = now(), updated_at = now(),
                    last_error = NULL
             WHERE repo_id = $1 AND from_node = $2 AND to_node = $3 AND state = 'copying'",
        )
        .bind(repo)
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let assigned = if moved == 1 {
            reassign(&mut tx, repo, from, to).await?
        } else {
            false
        };
        if !assigned {
            tx.rollback().await?;
            self.cancel(
                repo,
                to,
                "the repository's assignment changed during the move",
            )
            .await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE gitserver_repos SET node = $2, repo_size_bytes = coalesce($3, repo_size_bytes),
                    updated_at = now()
             WHERE repo_id = $1",
        )
        .bind(repo)
        .bind(to)
        .bind(size_bytes)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE search_index_repos SET node = $2 WHERE repo_id = $1")
            .bind(repo)
            .bind(to)
            .execute(&mut *tx)
            .await?;
        notify(
            &mut *tx,
            channels::REPO_SHARDS,
            &crate::notify::repo_shards_payload(&[repo]),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Reassigns a repo whose old owner `from` is gone (or never cloned it) to `to` without a
    /// copy: the move is `done` and the repo is marked not cloned, for the caller to queue a
    /// clone. Returns `false` (cancelling the move) when it no longer applies.
    pub async fn reassign_without_copy(&self, repo: RepoId, from: &str, to: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let moved = sqlx::query(
            "UPDATE repo_moves SET state = 'done', finished_at = now(), updated_at = now(),
                    last_error = NULL
             WHERE repo_id = $1 AND from_node = $2 AND to_node = $3 AND state = 'copying'",
        )
        .bind(repo)
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let assigned = moved == 1 && reassign(&mut tx, repo, from, to).await?;
        if !assigned {
            tx.rollback().await?;
            self.cancel(
                repo,
                to,
                "the repository's assignment changed during the move",
            )
            .await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE gitserver_repos SET clone_status = 'not_cloned', cloning_progress = NULL,
                    node = NULL, updated_at = now()
             WHERE repo_id = $1",
        )
        .bind(repo)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM search_index_repos WHERE repo_id = $1")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        notify(
            &mut *tx,
            channels::REPO_SHARDS,
            &crate::notify::repo_shards_payload(&[repo]),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    // ------------------------------------------------------------------ old owner

    /// Repos moved away from node `from` at least `grace` ago whose old copy is still there.
    pub async fn flipped_from(
        &self,
        from: &str,
        grace: Duration,
        limit: i64,
    ) -> Result<Vec<RepoId>> {
        Ok(sqlx::query_scalar(
            "SELECT repo_id FROM repo_moves
             WHERE from_node = $1 AND state = 'flipped'
               AND flipped_at <= now() - make_interval(secs => $2)
             ORDER BY flipped_at LIMIT $3",
        )
        .bind(from)
        .bind(grace.as_secs_f64())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks a flipped move `done` (the old copy is gone).
    pub async fn finish(&self, repo: RepoId) -> Result<()> {
        sqlx::query(
            "UPDATE repo_moves SET state = 'done', finished_at = now(), updated_at = now()
             WHERE repo_id = $1 AND state = 'flipped'",
        )
        .bind(repo)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Of the repos on node `me`'s disk (`on_disk`), those that are copies nobody needs: the
    /// repo is assigned to another node, which has it cloned, and no move to `me` is open nor
    /// a move away from `me` waiting for its grace period. Left behind by cancelled or failed
    /// moves and by moves whose old owner was down when they flipped.
    pub async fn stray_copies(&self, me: &str, on_disk: &[RepoId]) -> Result<Vec<RepoId>> {
        if on_disk.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT r.id FROM repo r JOIN gitserver_repos g ON g.repo_id = r.id
             WHERE r.id = ANY($1) AND r.deleted_at IS NULL AND r.shard IS NOT NULL
               AND r.shard <> $2 AND g.node = r.shard AND g.clone_status = 'cloned'
               AND NOT EXISTS (
                   SELECT 1 FROM repo_moves m WHERE m.repo_id = r.id
                     AND ((m.to_node = $2 AND m.state IN ('queued', 'copying'))
                          OR (m.from_node = $2 AND m.state = 'flipped')))
             ORDER BY r.id",
        )
        .bind(on_disk)
        .bind(me)
        .fetch_all(&self.pool)
        .await?)
    }

    // ------------------------------------------------------------------ site admin

    /// Moves per state.
    pub async fn counts(&self) -> Result<MoveCounts> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT state, count(*) FROM repo_moves GROUP BY state")
                .fetch_all(&self.pool)
                .await?;
        let mut c = MoveCounts::default();
        for (state, n) in rows {
            match state.as_str() {
                move_state::QUEUED => c.queued = n,
                move_state::COPYING => c.copying = n,
                move_state::FLIPPED => c.flipped = n,
                move_state::DONE => c.done = n,
                move_state::FAILED => c.failed = n,
                move_state::CANCELLED => c.cancelled = n,
                _ => {}
            }
        }
        Ok(c)
    }

    /// Moves in `states` (all when empty), the most recently updated first.
    pub async fn list(&self, states: &[String], limit: i64) -> Result<Vec<RepoMove>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repo_moves m JOIN repo r ON r.id = m.repo_id
             WHERE cardinality($1::text[]) = 0 OR m.state = ANY($1)
             ORDER BY m.updated_at DESC, m.repo_id LIMIT $2"
        ))
        .bind(states)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Queues failed moves again with a fresh attempt count. Returns how many.
    pub async fn retry_failed(&self) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE repo_moves SET state = 'queued', attempts = 0, next_attempt_at = NULL,
                    finished_at = NULL, updated_at = now()
             WHERE state = 'failed'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

/// The live window for readers that do not know the repo nodes' heartbeat interval: six
/// intervals of the default 10 s.
pub const REPO_NODE_LIVE_WINDOW: Duration = Duration::from_secs(60);

/// Nodes whose heartbeat is at most `window` old at `now`.
pub fn live_repo_nodes(
    heartbeats: &[RepoNodeHeartbeat],
    now: DateTime<Utc>,
    window: Duration,
) -> HashSet<String> {
    let window = chrono::Duration::from_std(window).unwrap_or(chrono::Duration::MAX);
    heartbeats
        .iter()
        .filter(|h| now.signed_duration_since(h.seen_at) <= window)
        .map(|h| h.addr.clone())
        .collect()
}

/// Why moves cannot be planned now: the configured nodes that are not alive or run with a
/// different node list (a rollout in progress). Empty when the topology is settled.
pub fn unsettled_repo_nodes(
    configured: &[String],
    heartbeats: &[RepoNodeHeartbeat],
    live: &HashSet<String>,
) -> Vec<String> {
    let mut want: Vec<String> = configured.to_vec();
    want.sort();
    want.dedup();
    let mut out = Vec::new();
    for node in &want {
        let agrees = heartbeats
            .iter()
            .find(|h| &h.addr == node)
            .is_some_and(|h| live.contains(node) && h.nodes == want);
        if !agrees {
            out.push(node.clone());
        }
    }
    out
}

/// Moves `repo.shard` from `from` to `to`; whether it was still assigned to `from`.
async fn reassign(
    tx: &mut Transaction<'static, Postgres>,
    repo: RepoId,
    from: &str,
    to: &str,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE repo SET shard = $3 WHERE id = $1 AND shard = $2 AND deleted_at IS NULL",
    )
    .bind(repo)
    .bind(from)
    .bind(to)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1)
}

/// A planning pass holding the planner lock (see [`RepoMoveStore::begin_planning`]). Dropping
/// it without [`commit`](Self::commit) discards its writes.
pub struct Planning {
    tx: Transaction<'static, Postgres>,
}

impl std::fmt::Debug for Planning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planning").finish_non_exhaustive()
    }
}

impl Planning {
    /// Every live repo's assignment (`repo.shard`), unassigned repos left out.
    pub async fn assignments(&mut self) -> Result<Vec<(RepoId, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, shard FROM repo WHERE shard IS NOT NULL AND deleted_at IS NULL",
        )
        .fetch_all(&mut *self.tx)
        .await?)
    }

    /// The live repos among `names` (matched case-insensitively), as `(lowercased name, id)`.
    pub async fn repo_ids_by_name(&mut self, names: &[String]) -> Result<Vec<(String, RepoId)>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        Ok(sqlx::query_as(
            "SELECT lower(name), id FROM repo WHERE lower(name) = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&lower)
        .fetch_all(&mut *self.tx)
        .await?)
    }

    /// Moves that are not finished (`queued`, `copying`, `flipped`, `failed`).
    pub async fn open_moves(&mut self) -> Result<Vec<OpenRepoMove>> {
        Ok(sqlx::query_as(
            "SELECT repo_id, from_node, to_node, state FROM repo_moves
             WHERE state IN ('queued', 'copying', 'flipped', 'failed')",
        )
        .fetch_all(&mut *self.tx)
        .await?)
    }

    /// Queues moves, replacing finished ones of the same repos. Returns how many.
    pub async fn queue(&mut self, moves: &[NewRepoMove]) -> Result<u64> {
        let mut n = 0;
        for chunk in moves.chunks(5_000) {
            let ids: Vec<RepoId> = chunk.iter().map(|m| m.repo_id).collect();
            let from: Vec<&str> = chunk.iter().map(|m| m.from_node.as_str()).collect();
            let to: Vec<&str> = chunk.iter().map(|m| m.to_node.as_str()).collect();
            let kind: Vec<&str> = chunk.iter().map(|m| m.kind.as_str()).collect();
            n += sqlx::query(
                "INSERT INTO repo_moves (repo_id, from_node, to_node, kind)
                 SELECT * FROM unnest($1::int[], $2::text[], $3::text[], $4::text[])
                 ON CONFLICT (repo_id) DO UPDATE
                 SET from_node = EXCLUDED.from_node, to_node = EXCLUDED.to_node,
                     kind = EXCLUDED.kind, state = 'queued', attempts = 0, bytes_copied = 0,
                     last_error = NULL, next_attempt_at = NULL, created_at = now(),
                     started_at = NULL, flipped_at = NULL, finished_at = NULL, updated_at = now()
                 WHERE repo_moves.state IN ('done', 'cancelled')",
            )
            .bind(&ids)
            .bind(&from)
            .bind(&to)
            .bind(&kind)
            .execute(&mut *self.tx)
            .await?
            .rows_affected();
        }
        Ok(n)
    }

    /// Cancels moves that are not flipped yet. Returns how many.
    pub async fn cancel(&mut self, repos: &[RepoId], reason: &str) -> Result<u64> {
        if repos.is_empty() {
            return Ok(0);
        }
        Ok(sqlx::query(
            "UPDATE repo_moves SET state = 'cancelled', last_error = $2, updated_at = now(),
                    finished_at = now()
             WHERE repo_id = ANY($1) AND state IN ('queued', 'copying', 'failed')",
        )
        .bind(repos)
        .bind(reason)
        .execute(&mut *self.tx)
        .await?
        .rows_affected())
    }

    /// Marks flipped moves `done` whose old owner is not among `live` nodes (it left, so its
    /// copy left with it).
    pub async fn finish_orphaned(&mut self, live: &[String]) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE repo_moves SET state = 'done', finished_at = now(), updated_at = now()
             WHERE state = 'flipped' AND NOT (from_node = ANY($1))",
        )
        .bind(live)
        .execute(&mut *self.tx)
        .await?
        .rows_affected())
    }

    /// Deletes finished moves (`done`, `cancelled`) last updated before `before`.
    pub async fn prune(&mut self, before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM repo_moves WHERE state IN ('done', 'cancelled') AND updated_at < $1",
        )
        .bind(before)
        .execute(&mut *self.tx)
        .await?
        .rows_affected())
    }

    /// Commits the pass.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}
