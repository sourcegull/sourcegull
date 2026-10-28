//! Ranking from the code graph, for site admins: `rankingSummary` reports the
//! PageRank runs (`GlobalRankingSummary`, with a run's statistics) and
//! `bumpDerivativeGraphKey` makes the next pass recompute ranks.

use async_graphql::{Context, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_ranking::store::{RankingRun, RankingStore};

use crate::context::{db_error, require_site_admin, services, to_int};
use sourcegull_graphql_core::EmptyResponse;

/// Runs listed in the summary.
const RUNS_LISTED: i64 = 10;

/// One ranking run.
#[derive(SimpleObject)]
pub struct RankingSummary {
    /// The run's id; the generation of the ranks it wrote.
    graph_key: String,
    /// Whether this run's ranks are the current ones (the newest completed run).
    visible_to_search: bool,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    failure_message: Option<String>,
    /// SCIP indexes whose references were exported.
    num_exported_indexes: i32,
    /// Files in the reference graph.
    num_paths: Option<i32>,
    /// File → file edges in the reference graph.
    num_edges: Option<i32>,
    /// PageRank iterations until convergence.
    iterations: Option<i32>,
    /// Repositories whose ranks the run changed.
    num_repositories_changed: Option<i32>,
}

fn summary(r: &RankingRun, current: Option<i64>) -> RankingSummary {
    RankingSummary {
        graph_key: r.id.to_string(),
        visible_to_search: current == Some(r.id),
        started_at: r.started_at,
        completed_at: r.finished_at,
        failure_message: r.failure_message.clone(),
        num_exported_indexes: r.num_uploads,
        num_paths: r.num_paths.map(to_int),
        num_edges: r.num_edges.map(to_int),
        iterations: r.iterations,
        num_repositories_changed: r.num_repos_changed,
    }
}

/// The progress of ranking.
#[derive(SimpleObject)]
pub struct GlobalRankingSummary {
    /// Recent runs, newest first.
    ranking_summary: Vec<RankingSummary>,
    /// The id of the run whose ranks are current.
    derivative_graph_key: Option<String>,
    /// Whether ranking is on (`codeIntelRanking.documentReferenceCountsEnabled`).
    enabled: bool,
    /// SCIP indexes the current ranks come from.
    num_exported_indexes: i32,
    /// Repositories with ranked files.
    num_ranked_repositories: i32,
    /// Files with a rank above 0.
    num_ranked_paths: i32,
}

#[derive(Default)]
pub struct RankingQuery;

#[Object]
impl RankingQuery {
    /// The progress of ranking from the code graph (site admins only).
    async fn ranking_summary(&self, ctx: &Context<'_>) -> Result<GlobalRankingSummary> {
        require_site_admin(ctx)?;
        let s = services(ctx)?;
        let store = RankingStore::new(&s.db);
        let runs = store.runs(RUNS_LISTED).await.map_err(db_error)?;
        let current = store.last_completed().await.map_err(db_error)?;
        let totals = store.totals().await.map_err(db_error)?;
        let current_id = current.as_ref().map(|r| r.id);
        Ok(GlobalRankingSummary {
            ranking_summary: runs.iter().map(|r| summary(r, current_id)).collect(),
            derivative_graph_key: current_id.map(|id| id.to_string()),
            enabled: s.config.load().value.codeintel_ranking_enabled,
            num_exported_indexes: current.map_or(0, |r| r.num_uploads),
            num_ranked_repositories: to_int(totals.repos),
            num_ranked_paths: to_int(totals.paths),
        })
    }
}

#[derive(Default)]
pub struct RankingMutation;

#[Object]
impl RankingMutation {
    /// Recomputes ranks at the next pass even if the uploads did not change (site admins
    /// only).
    async fn bump_derivative_graph_key(&self, ctx: &Context<'_>) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        RankingStore::new(&services(ctx)?.db)
            .request_rerun()
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }
}
