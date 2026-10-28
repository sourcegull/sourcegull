//! The ranking job: a leader-elected loop in the `worker` role.
//!
//! Each pass finds, per repository with completed uploads, the uploads serving its default
//! branch head (what `PreciseIndex.isLatestForRepo` reports: per root and indexer, the
//! nearest upload at or before the head, else just after it). When that set differs from
//! the one the last completed run read (or a rerun was requested), it exports each upload's
//! references, runs PageRank over the merged graph and stores every repository's ranks.
//! Repo nodes notice a changed generation at their next reconcile and rebuild those
//! repositories' shards.
//!
//! Repositories whose head does not resolve (empty or not cloned yet) have no serving
//! uploads; any other git or database error ends the pass, which is retried at the next
//! interval, so a passing outage never drops ranks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use sourcegull_codeintel_policies::PolicyError;
use sourcegull_codeintel_policies::git::RepoGit;
use sourcegull_codeintel_scip::GraphCache;
use sourcegull_codeintel_scip::store::{CodeIntelStore, Upload};
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{Db, DbError, Fence};
use sourcegull_rpc::RepoClients;
use sourcegull_services::git::GitCallError;
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::export::{UploadExport, assemble, export_upload};
use crate::pagerank::{Options, normalize, pagerank};
use crate::store::{RankingStore, RepoRanksPayload, RunStats};

/// Lease name of the ranking job.
pub const RANKER: &str = "codeintel-ranker";
/// Time between passes.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Disk budget of the ranking job's code graph cache.
pub const GRAPH_CACHE_BYTES: u64 = 4 << 30;
/// Code graphs fetched and exported at once.
const EXPORT_CONCURRENCY: usize = 4;
/// Counter of completed ranking runs.
pub const RUNS_METRIC: &str = metric_names::CODEINTEL_RANKING_RUNS_TOTAL;
/// Duration of the last ranking run.
pub const DURATION_METRIC: &str = metric_names::CODEINTEL_RANKING_DURATION_SECONDS;

#[derive(Debug, thiserror::Error)]
pub enum RankingError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("{repo}: {error}")]
    Git { repo: String, error: PolicyError },
    #[error("upload {upload}: {error}")]
    Graph { upload: i64, error: String },
    #[error("ranking task failed: {0}")]
    Task(String),
}

/// What a pass did.
#[derive(Debug, Clone, PartialEq)]
pub enum PassOutcome {
    /// `codeIntelRanking.documentReferenceCountsEnabled` is off.
    Disabled,
    /// No uploads serve a default branch head and no ranks are stored.
    NoUploads,
    /// The serving uploads are those the last run read.
    Unchanged,
    Ranked(RankingReport),
}

/// A completed run.
#[derive(Debug, Clone, PartialEq)]
pub struct RankingReport {
    pub run: i64,
    pub uploads: usize,
    pub paths: u64,
    pub edges: u64,
    pub iterations: u32,
    pub converged: bool,
    /// Repositories whose ranks changed (new, updated or removed).
    pub repos_changed: u64,
    pub elapsed: Duration,
}

/// The ranks computed from exported uploads.
#[derive(Debug, Clone, PartialEq)]
pub struct Computed {
    /// Repositories with at least one ranked file, by id.
    pub ranks: Vec<RepoRanksPayload>,
    pub paths: u64,
    pub edges: u64,
    pub iterations: u32,
    pub converged: bool,
}

/// Runs PageRank over `uploads` and encodes each repository's ranks (files ranked 0 are
/// left out of the payloads; a missing path reads as 0).
pub fn compute_ranks(uploads: &[(RepoId, UploadExport)], opts: &Options) -> Computed {
    let rg = assemble(uploads);
    let pr = pagerank(&rg.graph, opts);
    let ranks = normalize(&pr.scores);
    let mut out: Vec<RepoRanksPayload> = Vec::new();
    let mut i = 0;
    while i < rg.nodes.len() {
        let repo = rg.nodes[i].0;
        let mut paths = Vec::new();
        while i < rg.nodes.len() && rg.nodes[i].0 == repo {
            if sourcegull_types::ranking::quantize(ranks[i]) > 0 {
                paths.push((rg.nodes[i].1.clone(), ranks[i]));
            }
            i += 1;
        }
        if !paths.is_empty() {
            out.push(RepoRanksPayload {
                repo,
                num_paths: i32::try_from(paths.len()).unwrap_or(i32::MAX),
                payload: sourcegull_types::ranking::encode_ranks(&paths),
            });
        }
    }
    Computed {
        ranks: out,
        paths: u64::from(rg.graph.nodes()),
        edges: rg.graph.edges(),
        iterations: pr.iterations,
        converged: pr.converged,
    }
}

/// The ranking job's dependencies.
#[derive(Clone)]
pub struct Ranker {
    pub db: Db,
    pub clients: Arc<RepoClients>,
    pub graphs: Arc<GraphCache>,
    pub config: SiteConfigHandle,
    pub options: Options,
}

impl std::fmt::Debug for Ranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ranker")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// A fingerprint of a set of uploads: their ids and processing times (reprocessing rewrites
/// an upload's graph under the same id).
fn fingerprint(uploads: &[(RepoId, Upload)]) -> String {
    let mut keys: Vec<(i64, i64)> = uploads
        .iter()
        .map(|(_, u)| (u.id, u.finished_at.map_or(0, |t| t.timestamp_micros())))
        .collect();
    keys.sort_unstable();
    let mut h = Sha256::new();
    for (id, at) in keys {
        h.update(id.to_le_bytes());
        h.update(at.to_le_bytes());
    }
    hex::encode(h.finalize())
}

impl Ranker {
    /// The uploads serving each repository's default branch head, by upload id.
    pub async fn serving_uploads(&self) -> Result<Vec<(RepoId, Upload)>, RankingError> {
        let store = CodeIntelStore::new(&self.db);
        let mut out = Vec::new();
        for (repo, name) in RankingStore::new(&self.db).repos_with_uploads().await? {
            let git = RepoGit::new(self.clients.clone(), repo, &name);
            let git_err = |error| RankingError::Git {
                repo: name.clone(),
                error,
            };
            let head = match git.commit("HEAD").await {
                Ok(Some((oid, _))) => oid,
                Ok(None)
                | Err(PolicyError::Git(
                    GitCallError::RepoNotCloned | GitCallError::CloneInProgress,
                )) => continue,
                Err(e) => return Err(git_err(e)),
            };
            let visible = git.nearest_uploads(&store, &head).await.map_err(git_err)?;
            out.extend(visible.iter().map(|v| (repo, v.upload.clone())));
        }
        out.sort_by_key(|(_, u)| u.id);
        out.dedup_by_key(|(_, u)| u.id);
        Ok(out)
    }

    /// One pass, not fenced; `force` recomputes even when the serving uploads did not change.
    pub async fn pass(&self, force: bool) -> Result<PassOutcome, RankingError> {
        self.pass_fenced(force, &Fence::none(RANKER)).await
    }

    /// One pass that stores the ranks only while `fence` is current.
    pub async fn pass_fenced(
        &self,
        force: bool,
        fence: &Fence,
    ) -> Result<PassOutcome, RankingError> {
        if !self.config.load().value.codeintel_ranking_enabled {
            return Ok(PassOutcome::Disabled);
        }
        let start = Instant::now();
        let uploads = self.serving_uploads().await?;
        let store = RankingStore::new(&self.db);
        if uploads.is_empty() && store.totals().await?.repos == 0 {
            return Ok(PassOutcome::NoUploads);
        }
        let inputs = fingerprint(&uploads);
        let last = store.last_completed().await?;
        if !force && last.is_some_and(|r| r.inputs == inputs) {
            return Ok(PassOutcome::Unchanged);
        }
        let run = store
            .start_run(&inputs, i32::try_from(uploads.len()).unwrap_or(i32::MAX))
            .await?;
        let result = self.rank(&uploads).await;
        let computed = match result {
            Ok(c) => c,
            Err(e) => {
                store.fail_run(run, &e.to_string()).await?;
                return Err(e);
            }
        };
        let stats = RunStats {
            num_paths: i64::try_from(computed.paths).unwrap_or(i64::MAX),
            num_edges: i64::try_from(computed.edges).unwrap_or(i64::MAX),
            iterations: i32::try_from(computed.iterations).unwrap_or(i32::MAX),
        };
        let repos_changed = store.finish_run(run, stats, &computed.ranks, fence).await?;
        let elapsed = start.elapsed();
        metrics::counter!(RUNS_METRIC).increment(1);
        metrics::gauge!(DURATION_METRIC).set(elapsed.as_secs_f64());
        Ok(PassOutcome::Ranked(RankingReport {
            run,
            uploads: uploads.len(),
            paths: computed.paths,
            edges: computed.edges,
            iterations: computed.iterations,
            converged: computed.converged,
            repos_changed,
            elapsed,
        }))
    }

    /// Exports `uploads` (in order) and computes the ranks.
    async fn rank(&self, uploads: &[(RepoId, Upload)]) -> Result<Computed, RankingError> {
        // Owned values in the stream, so the pass future is `Send` (no borrows held across
        // the buffered futures).
        let graphs = self.graphs.clone();
        let exports: Vec<(RepoId, UploadExport)> = futures::stream::iter(uploads.to_vec())
            .map(move |(repo, upload)| {
                let graphs = graphs.clone();
                async move {
                    let id = upload.id;
                    let graph_err = |error: String| RankingError::Graph { upload: id, error };
                    let graph = graphs
                        .get(&upload)
                        .await
                        .map_err(|e| graph_err(e.to_string()))?;
                    let export = tokio::task::spawn_blocking(move || export_upload(&graph))
                        .await
                        .map_err(|e| RankingError::Task(e.to_string()))?
                        .map_err(|e| graph_err(e.to_string()))?;
                    Ok::<_, RankingError>((repo, export))
                }
            })
            .buffered(EXPORT_CONCURRENCY)
            .try_collect()
            .await?;
        let options = self.options;
        tokio::task::spawn_blocking(move || compute_ranks(&exports, &options))
            .await
            .map_err(|e| RankingError::Task(e.to_string()))
    }
}

/// Runs passes every `interval` (the first right away) until `cancel` fires, storing ranks
/// under `fence`. A pass also runs right after a rerun was requested, at the next interval.
pub async fn run_ranker(
    ranker: Ranker,
    interval: Duration,
    fence: Fence,
    cancel: CancellationToken,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let pass = sourcegull_db::record_run(RANKER, ranker.pass_fenced(false, &fence));
        let result = tokio::select! {
            () = cancel.cancelled() => return,
            r = pass => r,
        };
        match result {
            Ok(PassOutcome::Ranked(r)) => tracing::info!(
                run = r.run,
                uploads = r.uploads,
                paths = r.paths,
                edges = r.edges,
                iterations = r.iterations,
                converged = r.converged,
                repos_changed = r.repos_changed,
                secs = r.elapsed.as_secs_f64(),
                "ranked files by PageRank over the code graph"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "ranking pass failed"),
        }
    }
}
