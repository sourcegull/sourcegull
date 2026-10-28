//! The code intelligence tables: uploads (`codeintel_uploads` with their parts, monikers and
//! symbol postings) and index jobs (`codeintel_index_jobs`).

mod index_jobs;
mod uploads;

pub use index_jobs::{
    INDEX_JOBS_TABLE, IndexDockerStep, IndexJob, IndexJobRow, IndexJobStore, ListIndexJobs,
};
pub use uploads::{
    AddPart, CodeIntelStore, ListUploads, NewUpload, PackageMoniker, ProcessedUpload,
    UPLOADS_TABLE, Upload, UploadCiJob, UploadState, normalize_root, upload_graph_key,
    upload_index_key, upload_part_key,
};
