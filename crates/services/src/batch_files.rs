//! Where the files that server-side bulk change specs mount are served
//! (`/.api/files/batch-changes/…` and, for executors, `/.executors/files/batch-changes/…`);
//! the routes are in `sourcegull-http-api`.

/// Largest file accepted (10 MiB, the limit clients enforce before uploading).
pub const MAX_FILE_BYTES: usize = 10 << 20;

/// Path prefix of the downloads under `/.api`.
pub const API_PREFIX: &str = "/.api/files/batch-changes";

/// Path prefix of the downloads for executors.
pub const EXECUTOR_PREFIX: &str = "/.executors/files/batch-changes";

/// The download path of a file under `prefix` ([`API_PREFIX`] or [`EXECUTOR_PREFIX`]).
pub fn file_path(prefix: &str, batch_spec_rand_id: &str, file_rand_id: &str) -> String {
    format!("{prefix}/{batch_spec_rand_id}/{file_rand_id}")
}
