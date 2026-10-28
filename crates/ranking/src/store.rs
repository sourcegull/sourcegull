//! `ranking_runs` and `ranking_path_ranks`: PageRank runs over the code graph and the
//! file ranks they produced, one compact row per repository.

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Fence, Result};
use sourcegull_types::{PathRanks, RepoId};
use sqlx::PgPool;

/// Runs kept for the admin summary.
const RUNS_KEPT: i64 = 20;
/// Repositories written per statement.
const RANKS_BATCH: usize = 500;

/// A ranking run.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RankingRun {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Fingerprint of the uploads the run read.
    pub inputs: String,
    pub num_uploads: i32,
    pub num_paths: Option<i64>,
    pub num_edges: Option<i64>,
    pub iterations: Option<i32>,
    pub num_repos_changed: Option<i32>,
    pub failure_message: Option<String>,
}

/// What a finished run computed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStats {
    pub num_paths: i64,
    pub num_edges: i64,
    pub iterations: i32,
}

/// One repository's ranks, encoded ([`sourcegull_types::ranking::encode_ranks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRanksPayload {
    pub repo: RepoId,
    pub num_paths: i32,
    pub payload: Vec<u8>,
}

/// Totals over the stored ranks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RanksTotals {
    pub repos: i64,
    pub paths: i64,
}

const RUN_COLUMNS: &str = "id, started_at, finished_at, inputs, num_uploads, num_paths, \
    num_edges, iterations, num_repos_changed, failure_message";

/// Store for ranking runs and ranks.
#[derive(Debug, Clone)]
pub struct RankingStore {
    pool: PgPool,
}

impl RankingStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Repositories (not deleted or blocked) with completed uploads, by id.
    pub async fn repos_with_uploads(&self) -> Result<Vec<(RepoId, String)>> {
        let rows: Vec<(i32, String)> = sqlx::query_as(
            "SELECT r.id, r.name FROM repo r
             WHERE r.deleted_at IS NULL AND r.blocked IS NULL AND EXISTS (
                SELECT 1 FROM codeintel_uploads u
                WHERE u.repository_id = r.id AND u.state = 'completed')
             ORDER BY r.id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id, n)| (RepoId(id), n)).collect())
    }

    /// The newest runs, newest first.
    pub async fn runs(&self, limit: i64) -> Result<Vec<RankingRun>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RUN_COLUMNS} FROM ranking_runs ORDER BY id DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The newest run that finished without error.
    pub async fn last_completed(&self) -> Result<Option<RankingRun>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RUN_COLUMNS} FROM ranking_runs
             WHERE finished_at IS NOT NULL AND failure_message IS NULL
             ORDER BY id DESC LIMIT 1"
        ))
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Records the start of a run over `num_uploads` uploads fingerprinted by `inputs`.
    pub async fn start_run(&self, inputs: &str, num_uploads: i32) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO ranking_runs (inputs, num_uploads) VALUES ($1, $2) RETURNING id",
        )
        .bind(inputs)
        .bind(num_uploads)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Marks a run failed.
    pub async fn fail_run(&self, id: i64, message: &str) -> Result<()> {
        sqlx::query(
            "UPDATE ranking_runs SET finished_at = now(), failure_message = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Finishes run `id` in one transaction: stores `ranks` (every repository with ranks),
    /// giving the run's id as generation to the repositories whose payload changed, deletes
    /// the ranks of repositories not in `ranks`, and records the statistics. Old runs are
    /// pruned. Returns the number of repositories whose ranks changed. Fails with
    /// [`sourcegull_db::DbError::Fenced`], storing nothing, when `fence` is no longer current.
    pub async fn finish_run(
        &self,
        id: i64,
        stats: RunStats,
        ranks: &[RepoRanksPayload],
        fence: &Fence,
    ) -> Result<u64> {
        let mut tx = fence.begin(&self.pool).await?;
        let mut changed = 0u64;
        for chunk in ranks.chunks(RANKS_BATCH) {
            let repos: Vec<i32> = chunk.iter().map(|r| r.repo.get()).collect();
            let counts: Vec<i32> = chunk.iter().map(|r| r.num_paths).collect();
            let payloads: Vec<&[u8]> = chunk.iter().map(|r| r.payload.as_slice()).collect();
            let n: i64 = sqlx::query_scalar(
                "WITH up AS (
                    INSERT INTO ranking_path_ranks (repository_id, generation, num_paths, payload)
                    SELECT t.repo, $1, t.n, t.payload
                    FROM unnest($2::int[], $3::int[], $4::bytea[]) AS t (repo, n, payload)
                    JOIN repo r ON r.id = t.repo
                    ON CONFLICT (repository_id) DO UPDATE SET
                        generation = EXCLUDED.generation, num_paths = EXCLUDED.num_paths,
                        payload = EXCLUDED.payload, updated_at = now()
                    WHERE ranking_path_ranks.payload IS DISTINCT FROM EXCLUDED.payload
                    RETURNING 1)
                 SELECT count(*) FROM up",
            )
            .bind(id)
            .bind(&repos)
            .bind(&counts)
            .bind(&payloads)
            .fetch_one(&mut *tx)
            .await?;
            changed += n as u64;
        }
        let all: Vec<i32> = ranks.iter().map(|r| r.repo.get()).collect();
        let removed =
            sqlx::query("DELETE FROM ranking_path_ranks WHERE NOT (repository_id = ANY($1))")
                .bind(&all)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        changed += removed;
        sqlx::query(
            "UPDATE ranking_runs SET finished_at = now(), num_paths = $2, num_edges = $3,
                iterations = $4, num_repos_changed = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(stats.num_paths)
        .bind(stats.num_edges)
        .bind(stats.iterations)
        .bind(i32::try_from(changed).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM ranking_runs WHERE id NOT IN (
                SELECT id FROM ranking_runs ORDER BY id DESC LIMIT $1)",
        )
        .bind(RUNS_KEPT)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// Makes the next pass recompute ranks even when the serving uploads did not change.
    pub async fn request_rerun(&self) -> Result<()> {
        sqlx::query(
            "UPDATE ranking_runs SET inputs = '' WHERE finished_at IS NOT NULL
               AND failure_message IS NULL",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The generation of a repository's ranks; `None` when it has none.
    pub async fn generation(&self, repo: RepoId) -> Result<Option<i64>> {
        Ok(
            sqlx::query_scalar(
                "SELECT generation FROM ranking_path_ranks WHERE repository_id = $1",
            )
            .bind(repo.get())
            .fetch_optional(&self.pool)
            .await?,
        )
    }

    /// A repository's ranks; `None` when it has none. A payload that does not decode is an
    /// error.
    pub async fn path_ranks(&self, repo: RepoId) -> Result<Option<PathRanks>> {
        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT generation, payload FROM ranking_path_ranks WHERE repository_id = $1",
        )
        .bind(repo.get())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(generation, payload)| {
            PathRanks::decode(generation, &payload)
                .map_err(|e| sourcegull_db::DbError::Sqlx(sqlx::Error::Decode(Box::new(e))))
        })
        .transpose()
    }

    /// How many repositories and paths have ranks.
    pub async fn totals(&self) -> Result<RanksTotals> {
        let (repos, paths): (i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(num_paths), 0)::bigint FROM ranking_path_ranks",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(RanksTotals { repos, paths })
    }
}
