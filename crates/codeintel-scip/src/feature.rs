//! SCIP upload processing as a server feature: the upload workers and the upload janitor.

use sourcegull_feature::{Feature, WorkerContext, WorkerTasks};

use crate::janitor;
use crate::repo_commits::RepoCommitChecker;
use crate::worker::{CommitChecker, upload_workers};

/// The `codeintel_uploads` workers with their resetter ([`upload_workers`]), which check commits
/// against the repository nodes when there are any ([`RepoCommitChecker`]), and the upload
/// janitor singleton ([`janitor::run_janitor`]).
pub struct UploadsFeature;

impl<S> Feature<S> for UploadsFeature {
    fn name(&self) -> &'static str {
        "codeintel-uploads"
    }

    fn worker_tasks(&self, ctx: &WorkerContext, tasks: &mut WorkerTasks) -> anyhow::Result<()> {
        {
            let (db, blobs) = (ctx.db.clone(), ctx.blobs.clone());
            tasks.singleton(janitor::UPLOAD_JANITOR, move |token, _fence| {
                janitor::run_janitor(db.clone(), blobs.clone(), token)
            });
        }
        let commits = ctx
            .repo_clients
            .clone()
            .map(|c| Box::new(RepoCommitChecker::new(c)) as Box<dyn CommitChecker>);
        let (w, resetter) = upload_workers(
            &ctx.db,
            ctx.blobs.clone(),
            ctx.codeintel_dir.clone(),
            &ctx.worker_id,
            commits,
        )?;
        tasks.worker(|token| w.run(token));
        tasks.worker(|token| resetter.run(token));
        Ok(())
    }
}
