//! The three search job queues, run by every worker process.
//!
//! - [`JobHandler`] (`exhaustive_search_jobs`): resolves the job's repositories as its
//!   creator would see them and adds a repo job per repository.
//! - [`RepoJobHandler`] (`exhaustive_search_repo_jobs`): expands the repository's revision
//!   specifiers (ref globs need its refs) and adds a revision job per revision.
//! - [`RevisionJobHandler`] (`exhaustive_search_repo_revision_jobs`): searches the repository
//!   at the revision as the creator, with no result limit but the job's (1,000,000 in all) and
//!   a timeout of one hour, and streams the results to the blob store. The blob is replaced
//!   only once complete, so a retried or reset job starts over cleanly; parts already written
//!   are never rewritten.
//!
//! Every queue retries transient failures (repo node or database outages) up to three times;
//! permanent ones (a deleted job or creator, a missing revision, a timeout) fail at once.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use sourcegull_authn::{Actor, AuthVia, PermissionSet};
use sourcegull_blobstore::Blobs;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::Db;
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_rpc::RepoClients;
use sourcegull_search::Limits;
use sourcegull_search::exhaustive::{max_results, plan_exhaustive};
use sourcegull_search::stream::{Event, SkippedReason};
use sourcegull_search_web::{RunOptions, SearchBackend, resolve_repositories, spawn_search};
use sourcegull_services::git::{GitCallError, GitReader};
use sourcegull_types::{RepoId, UserId};
use tokio::sync::mpsc;

use crate::export::blob_key;
use crate::revs::{DEFAULT_REVISION, expand, has_globs, ref_spec};
use crate::store::{JobPayload, RepoJobPayload, RevisionJobPayload, SearchJobStore};

/// Handlers per queue and worker process.
pub const WORKER_CONCURRENCY: usize = 5;

/// Bytes of results buffered before they go to the blob store.
const UPLOAD_CHUNK: usize = 1024 * 1024;

/// What the handlers share.
#[derive(Debug, Clone)]
pub struct SearchJobRunner {
    pub db: Db,
    pub store: SearchJobStore,
    pub search: SearchBackend,
    pub blobs: Blobs,
}

impl SearchJobRunner {
    /// A runner searching through `clients`, with permissions from a checker of its own.
    pub fn new(db: Db, config: SiteConfigHandle, clients: Arc<RepoClients>, blobs: Blobs) -> Self {
        let authz = sourcegull_authz::Checker::new(db.clone(), config);
        Self {
            store: SearchJobStore::new(db.pool().clone()),
            search: sourcegull_services::search_backend(db.clone(), authz, clients),
            db,
            blobs,
        }
    }

    /// The creator as a search actor (only identity and site admin status matter here).
    async fn actor(&self, user: UserId) -> Result<Actor, JobError> {
        let u = self
            .db
            .users()
            .get_by_id(user)
            .await
            .map_err(JobError::retry)?
            .ok_or_else(|| JobError::fatal("the user who created the search job was deleted"))?;
        Ok(Actor {
            user_id: u.id,
            username: u.username,
            site_admin: u.site_admin,
            via: AuthVia::Session,
            permissions: PermissionSet::default(),
        })
    }
}

/// Splits a job into repo jobs.
pub struct JobHandler(pub SearchJobRunner);

#[async_trait]
impl Handler<JobPayload> for JobHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<JobPayload>) -> Result<(), JobError> {
        let r = &self.0;
        let job = record.payload;
        let actor = r.actor(job.initiator_id).await?;
        let opts = RunOptions {
            exhaustive: true,
            only: None,
        };
        let repos = resolve_repositories(&r.search, Some(&actor), &job.query, None, opts)
            .await
            .map_err(|e| {
                if e == "internal error" {
                    JobError::retry(e)
                } else {
                    JobError::fatal(e)
                }
            })?;
        let rows: Vec<(i32, String)> = repos.iter().map(|r| (r.id, ref_spec(&r.revs))).collect();
        let added = r
            .store
            .add_repo_jobs(record.id, &rows)
            .await
            .map_err(JobError::retry)?;
        tracing::info!(
            job = record.id,
            repositories = repos.len(),
            added,
            "split search job into repository jobs"
        );
        Ok(())
    }
}

/// Splits a repo job into revision jobs.
pub struct RepoJobHandler(pub SearchJobRunner);

#[async_trait]
impl Handler<RepoJobPayload> for RepoJobHandler {
    async fn handle(
        &self,
        _ctx: JobContext,
        record: Record<RepoJobPayload>,
    ) -> Result<(), JobError> {
        let r = &self.0;
        let job = record.payload;
        let Some((_, _, _, repo_name, canceled)) = r
            .store
            .repo_job_context(record.id)
            .await
            .map_err(JobError::retry)?
        else {
            return Err(JobError::fatal("the search job was deleted"));
        };
        if canceled {
            return Ok(());
        }
        let refs = if has_globs(&job.ref_spec) {
            let git = GitReader::from_parts(
                r.search.repo_clients.clone(),
                RepoId(job.repo_id),
                &repo_name,
            );
            git.list_refs(true, true, "")
                .await
                .map_err(git_error)?
                .refs
                .into_iter()
                .map(|r| r.name)
                .collect()
        } else {
            Vec::new()
        };
        let revisions = expand(&job.ref_spec, &refs);
        r.store
            .add_revision_jobs(record.id, &revisions)
            .await
            .map_err(JobError::retry)?;
        Ok(())
    }
}

fn git_error(e: GitCallError) -> JobError {
    match e {
        GitCallError::RevisionNotFound(_) | GitCallError::InvalidArgument(_) => JobError::fatal(e),
        other => JobError::retry(other),
    }
}

/// Searches one repository revision.
pub struct RevisionJobHandler(pub SearchJobRunner);

#[async_trait]
impl Handler<RevisionJobPayload> for RevisionJobHandler {
    async fn handle(
        &self,
        ctx: JobContext,
        record: Record<RevisionJobPayload>,
    ) -> Result<(), JobError> {
        let r = &self.0;
        let revision = record.payload.revision;
        let Some(job) = r
            .store
            .revision_context(record.id)
            .await
            .map_err(JobError::retry)?
        else {
            return Err(JobError::fatal("the search job was deleted"));
        };
        if job.canceled {
            return Ok(());
        }
        let actor = r.actor(job.initiator_id).await?;
        let limits = Limits::from_site(&r.search.config);
        let plan = plan_exhaustive(&job.query, Default::default(), &limits, chrono::Utc::now())
            .map_err(JobError::fatal)?;
        let written = r
            .store
            .results_besides(job.search_job_id, record.id)
            .await
            .map_err(JobError::retry)?;
        let limit = max_results(&plan).unwrap_or(limits.exhaustive_max_results);
        let remaining = limit.saturating_sub(usize::try_from(written).unwrap_or(usize::MAX));
        let key = blob_key(job.search_job_id, record.id);
        if remaining == 0 {
            r.blobs.delete(&key).await.map_err(JobError::retry)?;
            let note = format!("not searched: the search job reached its limit of {limit} results");
            r.store
                .set_result_count(record.id, 0, Some(&note))
                .await
                .map_err(JobError::retry)?;
            return Ok(());
        }

        // A missing revision fails here, before any search; an empty repository has no
        // default branch and nothing to search.
        let git = GitReader::from_parts(
            r.search.repo_clients.clone(),
            RepoId(job.repo_id),
            &job.repo_name,
        );
        match git.resolve_revision(&revision).await {
            Ok(_) => {}
            Err(GitCallError::RevisionNotFound(_)) if revision == DEFAULT_REVISION => {
                r.store
                    .set_result_count(record.id, 0, None)
                    .await
                    .map_err(JobError::retry)?;
                return Ok(());
            }
            Err(e) => return Err(git_error(e)),
        }

        let run = RunOptions {
            exhaustive: true,
            only: Some((job.repo_id, revision.clone())),
        };
        let (events, search) = spawn_search(
            &r.search,
            Some(&actor),
            &job.query,
            None,
            Some(remaining),
            run,
        );
        let (chunks, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        let upload = {
            let (blobs, key) = (r.blobs.clone(), key.clone());
            let mut rx = rx;
            let body = futures::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed();
            tokio::spawn(async move { blobs.put(&key, body).await })
        };
        let outcome = tokio::select! {
            o = tokio::time::timeout(limits.exhaustive_timeout, write_results(events, &chunks)) => match o {
                Ok(o) => o,
                Err(_) => Err(JobError::fatal(format!(
                    "the search timed out after {}s",
                    limits.exhaustive_timeout.as_secs()
                ))),
            },
            () = ctx.cancelled() => Err(JobError::retry("canceled")),
        };
        search.abort();
        let count = match outcome {
            Ok(count) => count,
            Err(e) => {
                // The upload fails and leaves no partial blob.
                let _ = chunks.send(Err(io::Error::other(e.to_string()))).await;
                drop(chunks);
                let _ = upload.await;
                return Err(e);
            }
        };
        drop(chunks);
        upload
            .await
            .map_err(JobError::retry)?
            .map_err(JobError::retry)?;
        let note = (count >= remaining as u64)
            .then(|| format!("stopped at the search job's limit of {limit} results"));
        r.store
            .set_result_count(record.id, count as i64, note.as_deref())
            .await
            .map_err(JobError::retry)?;
        // Deleted while searching: the delete may have listed the blobs before this one.
        if r.store
            .initiator(job.search_job_id)
            .await
            .map_err(JobError::retry)?
            .is_none()
        {
            let _ = r.blobs.delete(&key).await;
        }
        Ok(())
    }
}

/// Writes the search's matches to `chunks` as JSON lines until it ends; returns how many.
async fn write_results(
    mut events: mpsc::Receiver<Event>,
    chunks: &mpsc::Sender<io::Result<Bytes>>,
) -> Result<u64, JobError> {
    let mut buf = BytesMut::new();
    let mut count = 0u64;
    let mut failure = None;
    let mut done = false;
    while let Some(event) = events.recv().await {
        match event {
            Event::Matches(matches) => {
                for m in &matches {
                    let line = serde_json::to_vec(m).map_err(JobError::fatal)?;
                    buf.extend_from_slice(&line);
                    buf.extend_from_slice(b"\n");
                    count += 1;
                }
                if buf.len() >= UPLOAD_CHUNK && chunks.send(Ok(buf.split().freeze())).await.is_err()
                {
                    return Err(JobError::retry("the blob store upload stopped"));
                }
            }
            Event::Progress(p) => {
                done = true;
                for s in &p.skipped {
                    let message = format!("{}: {}", s.title, s.message);
                    failure = match s.reason {
                        SkippedReason::RepositoryMissing | SkippedReason::ShardTimeout => {
                            Some(JobError::fatal(message))
                        }
                        SkippedReason::Error | SkippedReason::RepositoryCloning => {
                            Some(JobError::retry(message))
                        }
                        _ => failure,
                    };
                }
            }
            // The query was valid when the job was created; an alert now is permanent.
            Event::Alert(a) => {
                failure = Some(JobError::fatal(format!("{}: {}", a.title, a.description)))
            }
            Event::Error(e) => failure = Some(JobError::retry(e)),
            Event::Filters(_) | Event::Done => {}
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if !done {
        return Err(JobError::retry("the search ended without finishing"));
    }
    if !buf.is_empty() && chunks.send(Ok(buf.freeze())).await.is_err() {
        return Err(JobError::retry("the blob store upload stopped"));
    }
    Ok(count)
}

/// A worker and a resetter per search jobs queue, ready to run.
pub struct SearchJobWorkers {
    pub jobs: (Worker<JobPayload>, Resetter<JobPayload>),
    pub repo_jobs: (Worker<RepoJobPayload>, Resetter<RepoJobPayload>),
    pub revision_jobs: (Worker<RevisionJobPayload>, Resetter<RevisionJobPayload>),
}

/// The workers of the three queues with their stalled-job resetters.
pub fn search_job_workers(
    runner: SearchJobRunner,
    worker_id: &str,
) -> Result<SearchJobWorkers, sourcegull_pgqueue::Error> {
    let pool = runner.db.pool().clone();
    let opts = || {
        let mut o = WorkerOptions::new(worker_id);
        o.concurrency = WORKER_CONCURRENCY;
        o
    };
    let reset = Duration::from_secs(30);
    let q: Queue<JobPayload> = Queue::new(pool.clone(), SearchJobStore::jobs_queue_config())?;
    let jobs = (
        Worker::new(q.clone(), JobHandler(runner.clone()), opts()),
        Resetter::new(q, reset),
    );
    let q: Queue<RepoJobPayload> =
        Queue::new(pool.clone(), SearchJobStore::repo_jobs_queue_config())?;
    let repo_jobs = (
        Worker::new(q.clone(), RepoJobHandler(runner.clone()), opts()),
        Resetter::new(q, reset),
    );
    let q: Queue<RevisionJobPayload> =
        Queue::new(pool, SearchJobStore::revision_jobs_queue_config())?;
    let revision_jobs = (
        Worker::new(q.clone(), RevisionJobHandler(runner), opts()),
        Resetter::new(q, reset),
    );
    Ok(SearchJobWorkers {
        jobs,
        repo_jobs,
        revision_jobs,
    })
}

impl SearchJobWorkers {
    /// Runs all workers and resetters until `cancel` fires.
    pub async fn run(self, cancel: tokio_util::sync::CancellationToken) {
        tokio::join!(
            self.jobs.0.run(cancel.clone()),
            self.jobs.1.run(cancel.clone()),
            self.repo_jobs.0.run(cancel.clone()),
            self.repo_jobs.1.run(cancel.clone()),
            self.revision_jobs.0.run(cancel.clone()),
            self.revision_jobs.1.run(cancel),
        );
    }
}
