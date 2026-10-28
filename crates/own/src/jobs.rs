//! The background indexers of the ownership signals and analytics, on the pgqueue table
//! `own_background_jobs`:
//!
//! - `recent-contributors` (per repository): asks the repository's node for the commits of the
//!   last 90 days reachable from HEAD with their changed files (`RecentContributions`, read from
//!   the commit metadata log and its changes file) and applies the difference to the stored
//!   window ([`SignalStore::sync_contributions`]).
//! - `analytics` (per repository): lists the default branch's files and computes the
//!   CODEOWNERS and assignment coverage of each directory ([`crate::signals::path_stats`]).
//! - `recent-views` (global): counts the new `ViewBlob` events ([`SignalStore::index_views`]).
//!
//! The [scheduler](run_scheduler) (a leader-elected singleton in the `worker` role) queues
//! repository jobs when refs move (it follows `repo_ref_events`) and once a day per
//! repository (so commits leave the window), and the views job every few minutes, for the
//! signals that are enabled and not excluded for the repository. A job for a disabled signal
//! or an excluded repository clears that repository's data instead.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sourcegull_db::{Db, DbError};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, QueueConfig, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_rpc::RepoClients;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_types::RepoId;
use sqlx::{PgPool, Row};
use tokio_util::sync::CancellationToken;

use crate::codeowners::MAX_CODEOWNERS_BYTES;
use crate::filter::RepoOwners;
use crate::signals::{
    ANALYTICS, Contribution, RECENT_CONTRIBUTORS, RECENT_VIEWS, SignalStore, SyncStats, path_stats,
    window_start,
};
use crate::store::OwnStore;
use crate::{codeowners_cache, codeowners_paths};

/// The queue table.
pub const JOBS_TABLE: &str = "own_background_jobs";
/// Lease name of the scheduler.
pub const SCHEDULER: &str = "own-signals-scheduler";
/// How often a repository is indexed again without ref changes.
pub const REFRESH_INTERVAL: chrono::TimeDelta = chrono::TimeDelta::hours(24);

/// A queued indexer run.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OwnJob {
    pub repo_id: Option<i32>,
    pub job_type: String,
}

/// The queue's settings.
pub fn queue_config() -> QueueConfig {
    let mut c = QueueConfig::new(JOBS_TABLE);
    c.max_failures = 3;
    c.backoff_base = Duration::from_secs(60);
    c
}

/// Queues a job unless the same one is already queued. Returns whether one was added.
pub async fn enqueue(pool: &PgPool, repo: Option<RepoId>, job_type: &str) -> Result<bool, DbError> {
    let r = sqlx::query(
        "INSERT INTO own_background_jobs (repo_id, job_type) VALUES ($1, $2) \
         ON CONFLICT ((COALESCE(repo_id, 0)), job_type) WHERE state = 'queued' DO NOTHING",
    )
    .bind(repo.map(|r| r.get()))
    .bind(job_type)
    .execute(pool)
    .await?;
    if r.rows_affected() > 0 {
        sourcegull_pgqueue::notify_enqueued(pool, JOBS_TABLE)
            .await
            .map_err(|e| match e {
                sourcegull_pgqueue::Error::Db(e) => DbError::from(e),
                _ => DbError::Invalid("queue notification failed"),
            })?;
    }
    Ok(r.rows_affected() > 0)
}

/// Why an indexer run failed.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Client(#[from] sourcegull_rpc::ClientError),
    #[error("repo node: {0}")]
    Rpc(#[from] sourcegull_rpc::tonic::Status),
}

impl From<sqlx::Error> for IndexError {
    fn from(e: sqlx::Error) -> Self {
        IndexError::Db(e.into())
    }
}

/// What an indexer run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The signal is off or the repository excluded: its data was cleared.
    Skipped,
    /// The repository no longer exists.
    Gone,
    Contributions(SyncStats),
    /// Directories with coverage.
    Analytics {
        dirs: usize,
        files: usize,
        truncated: bool,
    },
    /// View events counted.
    Views(u64),
}

/// Runs the indexers.
#[derive(Clone)]
pub struct Indexer {
    db: Db,
    clients: Arc<RepoClients>,
}

impl Indexer {
    pub fn new(db: Db, clients: Arc<RepoClients>) -> Self {
        Self { db, clients }
    }

    fn signals(&self) -> SignalStore {
        SignalStore::new(&self.db)
    }

    /// The name and lower-case service type of a repository that is not deleted.
    async fn repo_name(&self, repo: RepoId) -> Result<Option<(String, String)>, IndexError> {
        Ok(sqlx::query_as(
            "SELECT name, external_service_type FROM repo WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(repo.get())
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Runs `job` at `now`.
    pub async fn run(&self, job: &OwnJob, now: DateTime<Utc>) -> Result<Outcome, IndexError> {
        let repo = job.repo_id.map(RepoId);
        match (job.job_type.as_str(), repo) {
            (RECENT_CONTRIBUTORS, Some(r)) => self.recent_contributors(r, now).await,
            (ANALYTICS, Some(r)) => self.analytics(r, now).await,
            (RECENT_VIEWS, _) => self.recent_views(now).await,
            _ => Ok(Outcome::Skipped),
        }
    }

    /// Syncs the recent contributions of `repo` with its node.
    pub async fn recent_contributors(
        &self,
        repo: RepoId,
        now: DateTime<Utc>,
    ) -> Result<Outcome, IndexError> {
        let Some((name, _)) = self.repo_name(repo).await? else {
            return Ok(Outcome::Gone);
        };
        let signals = self.signals();
        if !signals.configs().await?.applies(RECENT_CONTRIBUTORS, &name) {
            signals.clear_contributions(repo).await?;
            return Ok(Outcome::Skipped);
        }
        let since = window_start(now);
        let mut stream = self
            .clients
            .for_repo(repo)?
            .recent_contributions(pb::RecentContributionsRequest {
                repo: Some(pb::RepoRef {
                    id: repo.get(),
                    name,
                }),
                since: since.timestamp(),
            })
            .await?
            .into_inner();
        let mut window = Vec::new();
        while let Some(msg) = stream.message().await? {
            for c in msg.commits {
                window.push(Contribution {
                    commit: c.oid,
                    author_name: c.author_name,
                    author_email: c.author_email,
                    authored_at: Utc.timestamp_opt(c.author_time, 0).single().unwrap_or(now),
                    paths: c.paths,
                });
            }
        }
        let stats = signals.sync_contributions(repo, &window).await?;
        signals.mark_indexed(repo, RECENT_CONTRIBUTORS, now).await?;
        Ok(Outcome::Contributions(stats))
    }

    /// Computes the ownership coverage of `repo`'s default branch.
    pub async fn analytics(&self, repo: RepoId, now: DateTime<Utc>) -> Result<Outcome, IndexError> {
        let Some((name, service_type)) = self.repo_name(repo).await? else {
            return Ok(Outcome::Gone);
        };
        let signals = self.signals();
        if !signals.configs().await?.applies(ANALYTICS, &name) {
            signals.clear_path_stats(repo).await?;
            return Ok(Outcome::Skipped);
        }
        let r = Some(pb::RepoRef {
            id: repo.get(),
            name,
        });
        let mut client = self.clients.for_repo(repo)?;
        let head = match client
            .resolve_revision(pb::ResolveRevisionRequest {
                repo: r.clone(),
                rev: "HEAD".into(),
            })
            .await
        {
            Ok(h) => h.into_inner().commit_oid,
            // An empty repository has no files.
            Err(s) if s.code() == sourcegull_rpc::tonic::Code::NotFound => {
                signals
                    .save_path_stats(repo, &path_stats([], &RepoOwners::default()))
                    .await?;
                signals.mark_indexed(repo, ANALYTICS, now).await?;
                return Ok(Outcome::Analytics {
                    dirs: 1,
                    files: 0,
                    truncated: false,
                });
            }
            Err(s) => return Err(s.into()),
        };
        let mut stream = client
            .list_tree(pb::ListTreeRequest {
                repo: r.clone(),
                commit_oid: head.clone(),
                path: String::new(),
                recursive: true,
                include_sizes: false,
                limit: 0,
            })
            .await?
            .into_inner();
        let mut files = Vec::new();
        let mut truncated = false;
        while let Some(msg) = stream.message().await? {
            truncated |= msg.truncated;
            files.extend(msg.entries.into_iter().filter_map(|e| {
                matches!(
                    pb::EntryKind::try_from(e.kind),
                    Ok(pb::EntryKind::File | pb::EntryKind::Executable | pb::EntryKind::Symlink)
                )
                .then_some(e.path)
            }));
        }

        let own = OwnStore::new(&self.db);
        let codeowners = match own.codeowners_for_repo(repo).await? {
            Some(f) => Some(codeowners_cache().get_text(&f.contents)),
            None => {
                let mut found = None;
                for &path in codeowners_paths(&service_type) {
                    let entry = match client
                        .stat(pb::StatRequest {
                            repo: r.clone(),
                            commit_oid: head.clone(),
                            path: path.to_owned(),
                        })
                        .await
                    {
                        Ok(e) => e.into_inner().entry,
                        Err(s) if s.code() == sourcegull_rpc::tonic::Code::NotFound => continue,
                        Err(s) => return Err(s.into()),
                    };
                    let Some(entry) = entry else { continue };
                    if entry.kind == pb::EntryKind::Directory as i32 {
                        continue;
                    }
                    let compiled = match codeowners_cache().get(&entry.oid) {
                        Some(c) => c,
                        None => {
                            let file = client
                                .read_file(pb::ReadFileRequest {
                                    repo: r.clone(),
                                    commit_oid: head.clone(),
                                    path: path.to_owned(),
                                    max_bytes: MAX_CODEOWNERS_BYTES as u64,
                                })
                                .await?
                                .into_inner();
                            let text = String::from_utf8_lossy(&file.content).into_owned();
                            codeowners_cache().get_or_compile(&entry.oid, || text)
                        }
                    };
                    found = Some(compiled);
                    break;
                }
                found
            }
        };
        let assigned = own
            .assigned_entries(&[repo.get()])
            .await?
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        let owners = RepoOwners {
            codeowners,
            assigned,
        };
        let stats = path_stats(files.iter().map(String::as_str), &owners);
        signals.save_path_stats(repo, &stats).await?;
        signals.mark_indexed(repo, ANALYTICS, now).await?;
        Ok(Outcome::Analytics {
            dirs: stats.len(),
            files: files.len(),
            truncated,
        })
    }

    /// Counts new file views.
    pub async fn recent_views(&self, now: DateTime<Utc>) -> Result<Outcome, IndexError> {
        let signals = self.signals();
        let enabled = signals
            .configs()
            .await?
            .get(RECENT_VIEWS)
            .is_some_and(|c| c.enabled);
        if !enabled {
            return Ok(Outcome::Skipped);
        }
        Ok(Outcome::Views(signals.index_views(now).await?))
    }
}

struct OwnJobHandler {
    indexer: Indexer,
}

#[async_trait]
impl Handler<OwnJob> for OwnJobHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<OwnJob>) -> Result<(), JobError> {
        let job = &record.payload;
        match self.indexer.run(job, Utc::now()).await {
            Ok(outcome) => {
                tracing::debug!(
                    repo = job.repo_id,
                    job_type = %job.job_type,
                    ?outcome,
                    "ownership signal indexed"
                );
                Ok(())
            }
            Err(e) => Err(JobError::retry(e)),
        }
    }
}

/// The queue worker and its resetter.
pub fn workers(
    indexer: Indexer,
    worker_id: &str,
) -> Result<(Worker<OwnJob>, Resetter<OwnJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<OwnJob> = Queue::new(indexer.db.pool().clone(), queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = 2;
    let worker = Worker::new(queue.clone(), OwnJobHandler { indexer }, opts);
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}

/// Scheduler settings.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Time between passes.
    pub interval: Duration,
    /// Time between recent-views jobs.
    pub views_interval: Duration,
    /// Repositories queued for their daily refresh per pass and signal.
    pub refresh_batch: i64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            views_interval: Duration::from_secs(5 * 60),
            refresh_batch: 500,
        }
    }
}

/// One scheduler pass at `now`: repositories whose refs moved, repositories due for their
/// daily refresh, and the views job when `views_due`. Returns the jobs queued.
pub async fn schedule_pass(
    db: &Db,
    config: &SchedulerConfig,
    now: DateTime<Utc>,
    views_due: bool,
) -> Result<u64, DbError> {
    let pool = db.pool();
    let signals = SignalStore::new(db).configs().await?;
    let repo_signals: Vec<&str> = [RECENT_CONTRIBUTORS, ANALYTICS]
        .into_iter()
        .filter(|s| signals.get(s).is_some_and(|c| c.enabled))
        .collect();
    let mut queued = 0;

    // Ref changes since the last pass.
    let cursor: i64 = sqlx::query_scalar("SELECT last_event_id FROM own_ref_events_cursor")
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query(
        "SELECT e.repo_id, max(e.id) AS last, r.name FROM repo_ref_events e \
         JOIN repo r ON r.id = e.repo_id AND r.deleted_at IS NULL \
         WHERE e.id > $1 AND e.source = 'fetch' GROUP BY e.repo_id, r.name",
    )
    .bind(cursor)
    .fetch_all(pool)
    .await?;
    let mut last = cursor;
    for r in &rows {
        let repo = RepoId(r.try_get("repo_id")?);
        let name: String = r.try_get("name")?;
        last = last.max(r.try_get("last")?);
        for s in &repo_signals {
            if signals.applies(s, &name) && enqueue(pool, Some(repo), s).await? {
                queued += 1;
            }
        }
    }
    if last > cursor {
        sqlx::query("UPDATE own_ref_events_cursor SET last_event_id = $1")
            .bind(last)
            .execute(pool)
            .await?;
    }

    // Daily refresh (and first runs) of cloned repositories.
    for s in &repo_signals {
        let rows = sqlx::query(
            "SELECT r.id, r.name FROM repo r \
             JOIN gitserver_repos g ON g.repo_id = r.id AND g.clone_status = 'cloned' \
             LEFT JOIN own_repo_index_state s ON s.repo_id = r.id AND s.job_type = $1 \
             WHERE r.deleted_at IS NULL AND (s.indexed_at IS NULL OR s.indexed_at < $2) \
               AND NOT EXISTS (SELECT 1 FROM own_background_jobs j WHERE j.repo_id = r.id \
                   AND j.job_type = $1 AND j.state IN ('queued', 'processing', 'errored')) \
             ORDER BY s.indexed_at NULLS FIRST, r.id LIMIT $3",
        )
        .bind(s)
        .bind(now - REFRESH_INTERVAL)
        .bind(config.refresh_batch)
        .fetch_all(pool)
        .await?;
        for r in rows {
            let repo = RepoId(r.try_get("id")?);
            let name: String = r.try_get("name")?;
            if signals.applies(s, &name) && enqueue(pool, Some(repo), s).await? {
                queued += 1;
            }
        }
    }

    if views_due
        && signals.get(RECENT_VIEWS).is_some_and(|c| c.enabled)
        && enqueue(pool, None, RECENT_VIEWS).await?
    {
        queued += 1;
    }
    // Finished jobs are kept a week.
    sqlx::query(
        "DELETE FROM own_background_jobs WHERE finished_at < $1 \
         AND state IN ('completed', 'failed', 'canceled')",
    )
    .bind(now - chrono::TimeDelta::days(7))
    .execute(pool)
    .await?;
    Ok(queued)
}

/// Runs the scheduler until `cancel` fires.
pub async fn run_scheduler(db: Db, config: SchedulerConfig, cancel: CancellationToken) {
    let mut last_views: Option<std::time::Instant> = None;
    loop {
        let views_due = last_views.is_none_or(|t| t.elapsed() >= config.views_interval);
        match sourcegull_db::record_run(
            SCHEDULER,
            schedule_pass(&db, &config, Utc::now(), views_due),
        )
        .await
        {
            Ok(n) => {
                if views_due {
                    last_views = Some(std::time::Instant::now());
                }
                if n > 0 {
                    tracing::debug!(queued = n, "ownership signal jobs queued");
                }
            }
            Err(e) => tracing::warn!(error = %e, "ownership signals scheduler pass failed"),
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(config.interval) => {}
        }
    }
}
