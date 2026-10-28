//! Precise index coverage: for every repository, whether a completed SCIP upload serves its
//! default branch, and whether its latest index job or upload failed.
//!
//! A repository is in exactly one [`CoverageState`], decided in this order:
//! 1. `failing`: its latest index job or latest upload failed (`errored` or `failed`) after
//!    its newest completed upload finished;
//! 2. `up_to_date`: a completed upload is at the default branch tip, or its newest completed
//!    upload finished within the stale window;
//! 3. `stale`: it has completed uploads, none at the tip, the newest older than the window;
//! 4. `none`: no completed upload, and an indexing policy applies to it;
//! 5. `not_targeted`: no completed upload, and no indexing policy applies.

use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_types::RepoId;
use sqlx::PgPool;

use super::policies::{ListPolicies, PolicyStore};

/// A repository's precise index coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverageState {
    UpToDate,
    Stale,
    Failing,
    None,
    NotTargeted,
}

impl CoverageState {
    pub const ALL: [CoverageState; 5] = [
        Self::UpToDate,
        Self::Stale,
        Self::Failing,
        Self::None,
        Self::NotTargeted,
    ];

    /// The name used in SQL and metric labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up_to_date",
            Self::Stale => "stale",
            Self::Failing => "failing",
            Self::None => "none",
            Self::NotTargeted => "not_targeted",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Which repositories indexing policies apply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageTargets {
    /// A global indexing policy applies to every repository.
    All,
    /// Only these repositories.
    Repos(Vec<RepoId>),
}

impl CoverageTargets {
    fn as_sql(&self) -> (bool, Vec<i32>) {
        match self {
            Self::All => (true, Vec::new()),
            Self::Repos(ids) => (false, ids.iter().map(|r| r.get()).collect()),
        }
    }
}

/// Options shared by the coverage queries.
#[derive(Debug, Clone)]
pub struct CoverageOptions {
    pub targets: CoverageTargets,
    /// A completed upload not at the default branch tip is stale once it is older than this.
    pub stale_after: TimeDelta,
}

/// Repositories per coverage state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoverageCounts {
    pub up_to_date: i64,
    pub stale: i64,
    pub failing: i64,
    pub none: i64,
    pub not_targeted: i64,
    /// Failing repositories whose latest failure is an index job (the rest are uploads).
    pub failing_index_jobs: i64,
}

impl CoverageCounts {
    pub fn get(&self, state: CoverageState) -> i64 {
        match state {
            CoverageState::UpToDate => self.up_to_date,
            CoverageState::Stale => self.stale,
            CoverageState::Failing => self.failing,
            CoverageState::None => self.none,
            CoverageState::NotTargeted => self.not_targeted,
        }
    }
}

/// One repository's coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoCoverage {
    pub repo_id: RepoId,
    pub repo_name: String,
    pub state: CoverageState,
    /// The default branch tip the repo node last fetched.
    pub head_commit: Option<String>,
    /// The commit of the newest completed upload.
    pub indexed_commit: Option<String>,
    /// When that upload finished processing.
    pub indexed_at: Option<DateTime<Utc>>,
    /// Whether a completed upload is at the default branch tip.
    pub indexed_at_head: bool,
    /// The latest failure (index job or upload), when it is newer than the newest completed
    /// upload.
    pub failure_message: Option<String>,
    pub failed_at: Option<DateTime<Utc>>,
    /// `index_job` or `upload`.
    pub failure_source: Option<String>,
    /// The index job or upload id of the failure.
    pub failure_id: Option<i64>,
}

/// Filters of [`CoverageStore::list`].
#[derive(Debug, Clone, Default)]
pub struct ListCoverage {
    pub state: Option<CoverageState>,
    /// Case-insensitive substring of the repository name.
    pub query: Option<String>,
    pub offset: i64,
    pub limit: i64,
}

/// Per-repository coverage rows. `$1`: every repository is targeted, `$2`: targeted ids,
/// `$3`: the stale window in seconds.
const COVERAGE_CTE: &str = "
WITH cov AS (
    SELECT r.id, r.name, g.head_oid,
           u.commit AS indexed_commit, u.done_at AS indexed_at,
           (g.head_oid IS NOT NULL AND EXISTS (
               SELECT 1 FROM codeintel_uploads h
               WHERE h.repository_id = r.id AND h.commit = g.head_oid AND h.state = 'completed'
           )) AS at_head,
           f.failure_message, f.done_at AS failed_at, f.source AS failure_source,
           f.id AS failure_id,
           ($1 OR r.id = ANY($2)) AS targeted
    FROM repo r
    LEFT JOIN gitserver_repos g ON g.repo_id = r.id
    LEFT JOIN LATERAL (
        SELECT c.commit, coalesce(c.finished_at, c.uploaded_at) AS done_at
        FROM codeintel_uploads c
        WHERE c.repository_id = r.id AND c.state = 'completed'
        ORDER BY c.id DESC LIMIT 1
    ) u ON true
    LEFT JOIN LATERAL (
        SELECT x.* FROM (
            (SELECT 'index_job'::text AS source, j.id, j.state, j.failure_message,
                    coalesce(j.finished_at, j.queued_at) AS done_at
             FROM codeintel_index_jobs j WHERE j.repository_id = r.id
             ORDER BY j.id DESC LIMIT 1)
            UNION ALL
            (SELECT 'upload'::text, c.id, c.state, c.failure_message,
                    coalesce(c.finished_at, c.uploaded_at)
             FROM codeintel_uploads c
             WHERE c.repository_id = r.id
               AND c.state NOT IN ('uploading', 'deleting', 'deleted')
             ORDER BY c.id DESC LIMIT 1)
        ) x
        WHERE x.state IN ('errored', 'failed')
          AND (u.done_at IS NULL OR x.done_at > u.done_at)
        ORDER BY x.done_at DESC LIMIT 1
    ) f ON true
    WHERE r.deleted_at IS NULL
), states AS (
    SELECT cov.*,
           CASE
               WHEN failed_at IS NOT NULL THEN 'failing'
               WHEN indexed_commit IS NOT NULL
                    AND (at_head OR indexed_at >= now() - make_interval(secs => $3))
                   THEN 'up_to_date'
               WHEN indexed_commit IS NOT NULL THEN 'stale'
               WHEN targeted THEN 'none'
               ELSE 'not_targeted'
           END AS state
    FROM cov
)";

#[derive(sqlx::FromRow)]
struct Row {
    id: RepoId,
    name: String,
    head_oid: Option<String>,
    indexed_commit: Option<String>,
    indexed_at: Option<DateTime<Utc>>,
    at_head: bool,
    failure_message: Option<String>,
    failed_at: Option<DateTime<Utc>>,
    failure_source: Option<String>,
    failure_id: Option<i64>,
    state: String,
    total: i64,
}

/// Store for the coverage report (reads only).
#[derive(Debug, Clone)]
pub struct CoverageStore {
    pool: PgPool,
}

impl CoverageStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Which repositories the indexing policies apply to, as the auto-indexing scheduler sees
    /// them: none while auto-indexing is off, and global policies only when allowed.
    pub async fn targets(&self, enabled: bool, allow_global: bool) -> Result<CoverageTargets> {
        if !enabled {
            return Ok(CoverageTargets::Repos(Vec::new()));
        }
        let policies: Vec<_> = PolicyStore::from_pool(self.pool.clone())
            .list(&ListPolicies {
                for_indexing: Some(true),
                ..Default::default()
            })
            .await?
            .into_iter()
            .filter(|p| allow_global || !p.is_global())
            .collect();
        if policies.iter().any(|p| p.is_global()) {
            return Ok(CoverageTargets::All);
        }
        let mut ids: Vec<RepoId> = policies.iter().filter_map(|p| p.repo_id).collect();
        let patterned: Vec<_> = policies
            .iter()
            .filter(|p| p.repository_patterns.is_some())
            .collect();
        if !patterned.is_empty() {
            let repos: Vec<(RepoId, String)> =
                sqlx::query_as("SELECT id, name FROM repo WHERE deleted_at IS NULL")
                    .fetch_all(&self.pool)
                    .await?;
            ids.extend(
                repos
                    .into_iter()
                    .filter(|(id, name)| patterned.iter().any(|p| p.applies_to(*id, name)))
                    .map(|(id, _)| id),
            );
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(CoverageTargets::Repos(ids))
    }

    /// Repositories per state.
    pub async fn counts(&self, opts: &CoverageOptions) -> Result<CoverageCounts> {
        let (all, ids) = opts.targets.as_sql();
        let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(&format!(
            "{COVERAGE_CTE}
             SELECT count(*) FILTER (WHERE state = 'up_to_date'),
                    count(*) FILTER (WHERE state = 'stale'),
                    count(*) FILTER (WHERE state = 'failing'),
                    count(*) FILTER (WHERE state = 'none'),
                    count(*) FILTER (WHERE state = 'not_targeted'),
                    count(*) FILTER (WHERE state = 'failing' AND failure_source = 'index_job')
             FROM states"
        ))
        .bind(all)
        .bind(ids)
        .bind(stale_secs(opts.stale_after))
        .fetch_one(&self.pool)
        .await?;
        Ok(CoverageCounts {
            up_to_date: row.0,
            stale: row.1,
            failing: row.2,
            none: row.3,
            not_targeted: row.4,
            failing_index_jobs: row.5,
        })
    }

    /// A page of repositories matching `filter`, ordered by name, and the number matching
    /// (0 when the page is past the end).
    pub async fn list(
        &self,
        opts: &CoverageOptions,
        filter: &ListCoverage,
    ) -> Result<(Vec<RepoCoverage>, i64)> {
        let (all, ids) = opts.targets.as_sql();
        let pattern = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| {
                format!(
                    "%{}%",
                    q.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });
        let page: Vec<Row> = sqlx::query_as(&format!(
            "{COVERAGE_CTE}
             SELECT id, name, head_oid, indexed_commit, indexed_at, at_head, failure_message,
                    failed_at, failure_source, failure_id, state, count(*) OVER () AS total
             FROM states
             WHERE ($4::text IS NULL OR state = $4)
               AND ($5::text IS NULL OR name ILIKE $5)
             ORDER BY lower(name), id
             OFFSET $6 LIMIT $7"
        ))
        .bind(all)
        .bind(&ids)
        .bind(stale_secs(opts.stale_after))
        .bind(filter.state.map(CoverageState::as_str))
        .bind(&pattern)
        .bind(filter.offset.max(0))
        .bind(filter.limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        let total = page.first().map_or(0, |r| r.total);
        let items = page
            .into_iter()
            .map(|r| RepoCoverage {
                repo_id: r.id,
                repo_name: r.name,
                state: CoverageState::parse(&r.state).unwrap_or(CoverageState::NotTargeted),
                head_commit: r.head_oid,
                indexed_commit: r.indexed_commit,
                indexed_at: r.indexed_at,
                indexed_at_head: r.at_head,
                failure_message: r.failure_message,
                failed_at: r.failed_at,
                failure_source: r.failure_source,
                failure_id: r.failure_id,
            })
            .collect();
        Ok((items, total))
    }
}

fn stale_secs(d: TimeDelta) -> f64 {
    d.num_seconds().max(0) as f64
}
