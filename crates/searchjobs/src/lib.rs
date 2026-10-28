//! Search Jobs: exhaustive searches run in the background.
//!
//! A user creates a job for a query that can run exhaustively ([`validate`], the rules of
//! [`sourcegull_search::exhaustive`]). The job is split in [`store`]'s three pgqueue queues:
//! repositories, then repository revisions, each revision searched on its own by the
//! [`worker`] as the job's creator, with the creator's repository permissions. Results go to
//! the blob store as JSONL, one blob per revision ([`export::blob_key`]); [`http`] serves
//! them concatenated (`/.api/search/export/{id}.jsonl`), converted to CSV (`{id}.csv`), and
//! the job's log (`{id}.log`). Only the creator and site admins see a job ([`access`]).
//! Cancel stops what has not finished; delete removes the job with its results; the
//! [`janitor`] applies `searchJobs.retention`. GraphQL lives in `sourcegull-graphql`.

pub mod access;
pub mod export;
mod feature;
pub mod http;
pub mod janitor;
pub mod revs;
pub mod store;
pub mod worker;

use sourcegull_blobstore::Blobs;
use sourcegull_query::PatternType;

pub use access::can_access;
pub use feature::SearchJobsFeature;
pub use store::{JobOrder, ListSearchJobs, RepoRevStats, SearchJob, SearchJobStore};

/// Errors of search job operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] sourcegull_db::DbError),
    #[error(transparent)]
    Blob(#[from] sourcegull_blobstore::Error),
}

/// Checks that `query` can run as a search job; the error is the message for the user.
/// Queries without `patterntype:` run as keyword queries.
pub fn validate(query: &str) -> Result<(), String> {
    sourcegull_search::exhaustive::plan_exhaustive(
        query,
        PatternType::default(),
        &sourcegull_search::Limits::default(),
        chrono::Utc::now(),
    )
    .map(drop)
    .map_err(|e| e.to_string())
}

/// Deletes job `id`: stops what still runs, deletes its result blobs (with `blobs`), then its
/// rows. A search finishing meanwhile deletes its own blob when it finds the job gone.
pub async fn delete_job(
    store: &SearchJobStore,
    blobs: Option<&Blobs>,
    id: i64,
) -> Result<(), Error> {
    store.cancel(id).await?;
    if let Some(blobs) = blobs {
        for key in blobs.list(&export::job_prefix(id)).await? {
            blobs.delete(&key).await?;
        }
    }
    store.delete(id).await?;
    Ok(())
}
