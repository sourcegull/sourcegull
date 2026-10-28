//! The upload processing queue: `codeintel_uploads` rows in `queued` are
//! processed by every `worker` process.
//!
//! A job downloads the index blob to a temporary directory, writes the code graph file next
//! to it on a blocking thread, stores the graph in the blob store and records the results
//! ([`CodeIntelStore::record_processed`]). An index that cannot be decoded fails the upload
//! at once with the decoder's message; storage and database errors are retried.
//!
//! First, the upload's commit must exist in the repository ([`CommitChecker`]). A commit the
//! repository node does not have yet (CI uploads often race the fetch) makes it fetch, and
//! the upload is retried with backoff; after the last attempt it fails with that message.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::store::{
    CodeIntelStore, PackageMoniker, ProcessedUpload, Upload, upload_graph_key, upload_index_key,
};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use sourcegull_blobstore::Blobs;
use sourcegull_db::Db;
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;

use crate::process::{ProcessError, process_index};

/// Uploads processed at once by one worker process (processing is CPU-bound).
pub const WORKER_CONCURRENCY: usize = 2;

/// Histogram of processing durations in seconds, by outcome.
pub const PROCESSING_SECONDS_METRIC: &str = metric_names::CODEINTEL_UPLOAD_PROCESSING_SECONDS;
/// Counter of index bytes processed (as uploaded, usually compressed).
pub const PROCESSED_BYTES_METRIC: &str = metric_names::CODEINTEL_UPLOAD_PROCESSED_BYTES_TOTAL;

/// Whether a commit exists in a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitStatus {
    Exists,
    /// The repository does not have the commit, even after fetching.
    Missing,
    /// The repository could not be asked (not cloned yet, node unavailable); the message
    /// says why.
    Unknown(String),
}

/// Checks that an upload's commit exists (repository nodes over gRPC in production).
#[async_trait]
pub trait CommitChecker: Send + Sync + 'static {
    /// Whether `commit` exists in `repo` (named `repo_name`); implementations fetch the
    /// repository before answering [`CommitStatus::Missing`].
    async fn check(&self, repo: RepoId, repo_name: &str, commit: &str) -> CommitStatus;
}

/// Processes queued uploads.
pub struct UploadHandler {
    db: Db,
    blobs: Blobs,
    work_dir: PathBuf,
    commits: Option<Box<dyn CommitChecker>>,
}

impl UploadHandler {
    /// `work_dir` holds each job's temporary files (the index and the graph being written).
    /// Without `commits` (no repository nodes), commits are not checked.
    pub fn new(
        db: Db,
        blobs: Blobs,
        work_dir: PathBuf,
        commits: Option<Box<dyn CommitChecker>>,
    ) -> Self {
        Self {
            db,
            blobs,
            work_dir,
            commits,
        }
    }

    async fn check_commit(&self, upload: &Upload) -> Result<(), JobError> {
        let Some(commits) = &self.commits else {
            return Ok(());
        };
        // A system job: the uploader's access was checked when the index was uploaded.
        let repos = self.db.repos();
        #[allow(clippy::disallowed_methods)]
        let repo = repos.get(upload.repo_id);
        let Some(repo) = repo.await.map_err(JobError::retry)? else {
            return Err(JobError::fatal("The repository no longer exists."));
        };
        match commits.check(repo.id, &repo.name, &upload.commit).await {
            CommitStatus::Exists => Ok(()),
            CommitStatus::Missing => Err(JobError::retry(format!(
                "Commit {} was not found in the repository.",
                upload.commit
            ))),
            CommitStatus::Unknown(why) => Err(JobError::retry(format!(
                "Could not check that commit {} exists: {why}",
                upload.commit
            ))),
        }
    }

    async fn process(&self, ctx: &JobContext, upload: &Upload) -> Result<(), JobError> {
        self.check_commit(upload).await?;
        tokio::fs::create_dir_all(&self.work_dir)
            .await
            .map_err(JobError::retry)?;
        let dir = tempfile::Builder::new()
            .prefix("upload-")
            .tempdir_in(&self.work_dir)
            .map_err(JobError::retry)?;
        let index = dir.path().join("index");
        let size = match sourcegull_blobstore::download(
            &*self.blobs,
            &upload_index_key(upload.id),
            &index,
        )
        .await
        {
            Ok(n) => n,
            Err(sourcegull_blobstore::Error::NotFound(_)) => {
                return Err(JobError::fatal(
                    "The uploaded index is missing from blob storage.",
                ));
            }
            Err(e) => return Err(JobError::retry(format!("downloading the index: {e}"))),
        };

        let graph = dir.path().join("graph");
        let (root, tmp, out) = (upload.root.clone(), dir.path().to_owned(), graph.clone());
        let cancel = ctx.cancellation().clone();
        let processed = tokio::task::spawn_blocking(move || {
            let input = std::fs::File::open(&index)
                .map_err(|e| ProcessError::Decode(crate::stream::DecodeError::Io(e)))?;
            process_index(input, &root, &out, &tmp, &|| cancel.is_cancelled())
        })
        .await
        .map_err(JobError::retry)?;
        let processed = match processed {
            Ok(p) => p,
            Err(ProcessError::Decode(e)) => return Err(JobError::fatal(e)),
            Err(e) => return Err(JobError::retry(e)),
        };

        let key = upload_graph_key(upload.id);
        let file = tokio::fs::File::open(&graph)
            .await
            .map_err(JobError::retry)?;
        let body = tokio_util::io::ReaderStream::with_capacity(file, 1 << 16)
            .map_err(std::io::Error::other)
            .boxed();
        self.blobs
            .put(&key, body)
            .await
            .map_err(|e| JobError::retry(format!("storing the code graph: {e}")))?;

        let g = &processed.graph;
        let postings: Vec<(i64, i16)> =
            g.postings.iter().map(|&(h, f)| (h, i16::from(f))).collect();
        let packages: Vec<PackageMoniker<'_>> = g
            .packages
            .iter()
            .map(|(kind, p)| PackageMoniker {
                kind: kind.as_str(),
                scheme: p.scheme.clone(),
                manager: p.manager.clone(),
                name: p.name.clone(),
                version: p.version.clone(),
            })
            .collect();
        let held = CodeIntelStore::new(&self.db)
            .record_processed(
                upload.id,
                ctx.worker_id(),
                &ProcessedUpload {
                    indexer: &processed.tool_name,
                    indexer_version: &processed.tool_version,
                    graph_key: &key,
                    num_documents: i32::try_from(g.num_documents).unwrap_or(i32::MAX),
                    num_occurrences: i64::try_from(g.num_occurrences).unwrap_or(i64::MAX),
                    num_symbols: i32::try_from(g.num_symbols).unwrap_or(i32::MAX),
                    postings: &postings,
                    packages: &packages,
                },
            )
            .await
            .map_err(JobError::retry)?;
        if !held {
            // Reset, canceled or deleted meanwhile; the janitor or the next run takes over.
            return Err(JobError::retry(
                "the upload is no longer held by this worker",
            ));
        }
        metrics::counter!(PROCESSED_BYTES_METRIC).increment(size);
        tracing::info!(
            upload = upload.id,
            documents = g.num_documents,
            occurrences = g.num_occurrences,
            symbols = g.num_symbols,
            skipped = processed.skipped_documents,
            index_bytes = size,
            graph_bytes = g.size,
            "processed upload"
        );
        Ok(())
    }
}

#[async_trait]
impl Handler<Upload> for UploadHandler {
    async fn handle(&self, ctx: JobContext, record: Record<Upload>) -> Result<(), JobError> {
        let started = Instant::now();
        let result = self.process(&ctx, &record.payload).await;
        let outcome = match &result {
            Ok(()) => "completed",
            Err(JobError::Fatal(_)) => "failed",
            Err(JobError::Retry(_)) => "errored",
        };
        metrics::histogram!(PROCESSING_SECONDS_METRIC, "outcome" => outcome)
            .record(started.elapsed().as_secs_f64());
        if let Err(e) = &result {
            tracing::warn!(upload = record.id, error = %e, "processing an upload failed");
        }
        result
    }
}

/// The upload processing worker and its stalled-job resetter, ready to run.
pub fn upload_workers(
    db: &Db,
    blobs: Blobs,
    work_dir: PathBuf,
    worker_id: &str,
    commits: Option<Box<dyn CommitChecker>>,
) -> Result<(Worker<Upload>, Resetter<Upload>), sourcegull_pgqueue::Error> {
    let queue: Queue<Upload> = Queue::new(db.pool().clone(), CodeIntelStore::queue_config())?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = WORKER_CONCURRENCY;
    let worker = Worker::new(
        queue.clone(),
        UploadHandler::new(db.clone(), blobs, work_dir, commits),
        opts,
    );
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
