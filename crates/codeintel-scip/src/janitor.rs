//! The upload janitor: a leader-elected loop in the `worker` role that removes what
//! deleted uploads leave behind.
//!
//! Each pass:
//! 1. marks uploads still `uploading` after [`ABANDONED_AFTER`] (multipart uploads the client
//!    gave up on), the uploads of deleted repositories, and expired uploads (no retention
//!    policy keeps them) that no other upload depends on as `deleting`;
//! 2. deletes the blobs (index, parts, code graph) of `deleting` uploads, then marks them
//!    `deleted`, dropping their symbol postings and package monikers;
//! 3. deletes blobs under `uploads/` whose upload is gone or `deleted` (a worker that stored a
//!    graph after the upload was deleted, or rows removed with their repository);
//! 4. drops the rows of uploads deleted more than [`DELETED_ROW_RETENTION`] ago.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::store::CodeIntelStore;
use sourcegull_blobstore::Blobs;
use sourcegull_db::{Db, DbError};
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

/// Lease name of the janitor.
pub const UPLOAD_JANITOR: &str = "codeintel-upload-janitor";
/// Time between passes.
pub const JANITOR_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Uploads still receiving parts after this long are abandoned.
pub const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 3600);
/// Rows of deleted uploads are kept this long (they show why an upload went away).
pub const DELETED_ROW_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
/// `deleting` uploads handled per batch.
const BATCH: i64 = 100;

/// Counter of uploads the janitor deleted, by reason of the deletion pass.
pub const JANITOR_DELETED_METRIC: &str = metric_names::CODEINTEL_JANITOR_UPLOADS_DELETED_TOTAL;
/// Counter of orphaned blobs the janitor deleted.
pub const JANITOR_ORPHANS_METRIC: &str =
    metric_names::CODEINTEL_JANITOR_ORPHANED_BLOBS_DELETED_TOTAL;

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JanitorReport {
    pub abandoned: u64,
    pub repo_deleted: u64,
    pub expired: u64,
    pub deleted: u64,
    pub orphaned_blobs: u64,
    pub purged_rows: u64,
}

/// Errors of a pass.
#[derive(Debug, thiserror::Error)]
pub enum JanitorError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("blob store: {0}")]
    Blobs(#[from] sourcegull_blobstore::Error),
}

/// Runs passes every [`JANITOR_INTERVAL`] (the first right away) until `cancel` fires.
pub async fn run_janitor(db: Db, blobs: Blobs, cancel: CancellationToken) {
    let mut tick = tokio::time::interval(JANITOR_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match sourcegull_db::record_run(UPLOAD_JANITOR, janitor_pass(&db, &blobs)).await {
            Ok(r) if r != JanitorReport::default() => {
                tracing::info!(
                    abandoned = r.abandoned,
                    repo_deleted = r.repo_deleted,
                    expired = r.expired,
                    deleted = r.deleted,
                    orphaned_blobs = r.orphaned_blobs,
                    purged_rows = r.purged_rows,
                    "upload janitor pass"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "upload janitor pass failed"),
        }
    }
}

/// One janitor pass.
pub async fn janitor_pass(db: &Db, blobs: &Blobs) -> Result<JanitorReport, JanitorError> {
    let store = CodeIntelStore::new(db);
    let now = chrono::Utc::now();
    let mut report = JanitorReport {
        abandoned: store.mark_abandoned(now - ago(ABANDONED_AFTER)).await?,
        repo_deleted: store.mark_repo_deleted().await?,
        expired: store.mark_expired_unreferenced().await?,
        ..JanitorReport::default()
    };
    metrics::counter!(JANITOR_DELETED_METRIC, "reason" => "abandoned").increment(report.abandoned);
    metrics::counter!(JANITOR_DELETED_METRIC, "reason" => "repository deleted")
        .increment(report.repo_deleted);
    metrics::counter!(JANITOR_DELETED_METRIC, "reason" => "expired").increment(report.expired);

    // Blobs by upload id, listed once per pass.
    let mut by_upload: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for key in blobs.list("uploads/").await? {
        if let Some(id) = key
            .strip_prefix("uploads/")
            .and_then(|rest| rest.split('/').next())
            .and_then(|id| id.parse::<i64>().ok())
        {
            by_upload.entry(id).or_default().push(key);
        }
    }

    loop {
        let batch = store.deleting(BATCH).await?;
        if batch.is_empty() {
            break;
        }
        for upload in &batch {
            for key in by_upload.remove(&upload.id).unwrap_or_default() {
                blobs.delete(&key).await?;
            }
            if store.mark_deleted(upload.id).await? {
                report.deleted += 1;
            }
        }
        if batch.len() < usize::try_from(BATCH).unwrap_or(usize::MAX) {
            break;
        }
    }

    let ids: Vec<i64> = by_upload.keys().copied().collect();
    let live = store.live_ids(&ids).await?;
    for (id, keys) in by_upload {
        if live.binary_search(&id).is_err() {
            for key in keys {
                blobs.delete(&key).await?;
                report.orphaned_blobs += 1;
            }
        }
    }
    metrics::counter!(JANITOR_ORPHANS_METRIC).increment(report.orphaned_blobs);

    report.purged_rows = store
        .purge_deleted(now - ago(DELETED_ROW_RETENTION))
        .await?;
    Ok(report)
}

fn ago(d: Duration) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(d).unwrap_or(chrono::TimeDelta::MAX)
}
