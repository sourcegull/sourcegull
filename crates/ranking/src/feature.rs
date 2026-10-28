//! Ranking as a server feature: the ranking job singleton.

use std::sync::Arc;

use anyhow::Context as _;
use sourcegull_codeintel_scip::GraphCache;
use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::job;

/// The ranking job ([`crate::run_ranker`]), with its code graph cache under
/// `ranking-graphs` in the code intelligence directory. Needs the repository nodes.
pub struct RankingFeature;

impl<S> Feature<S> for RankingFeature {
    fn name(&self) -> &'static str {
        "ranking"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        let Some(clients) = ctx.repo_clients.clone() else {
            return Ok(());
        };
        let dir = ctx.codeintel_dir.join("ranking-graphs");
        let graphs = GraphCache::new(&dir, ctx.blobs.clone(), job::GRAPH_CACHE_BYTES)
            .with_context(|| format!("creating the ranking graph cache in {}", dir.display()))?;
        let ranker = crate::Ranker {
            db: ctx.db.clone(),
            clients,
            graphs: Arc::new(graphs),
            config: ctx.config.clone(),
            options: crate::Options::default(),
        };
        tasks.singleton(job::RANKER, move |token, fence| {
            crate::run_ranker(ranker.clone(), job::DEFAULT_INTERVAL, fence, token)
        });
        Ok(())
    }
}
