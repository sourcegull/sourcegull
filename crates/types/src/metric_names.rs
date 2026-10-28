//! The names of the Prometheus metrics Sourcegull exports, shared by the code that records them and
//! the dashboards and alerts (`sourcegull-monitoring`), whose tests check that every query uses
//! only names in [`ALL`].

// analytics
/// Counter of days rolled up.
pub const EVENT_LOGS_DAYS_ROLLED_UP_TOTAL: &str = "sourcegull_event_logs_days_rolled_up_total";
/// Counter of raw usage events deleted.
pub const EVENT_LOGS_PRUNED_TOTAL: &str = "sourcegull_event_logs_pruned_total";

// autoindex
/// Counter of commits considered for queueing, by trigger (`event`, `sweep`).
pub const AUTOINDEX_COMMITS_CONSIDERED_TOTAL: &str =
    "sourcegull_autoindex_commits_considered_total";
/// Counter of index jobs queued.
pub const AUTOINDEX_JOBS_QUEUED_TOTAL: &str = "sourcegull_autoindex_jobs_queued_total";
/// Counter of commits not queued because of the per-repository rate limit.
pub const AUTOINDEX_RATE_LIMITED_TOTAL: &str = "sourcegull_autoindex_rate_limited_total";

// batches-reconciler
/// Counter of processed bulk operation jobs, by `type` and `outcome` (`ok`, `retry`,
/// `failed`).
pub const BATCHES_BULK_JOBS_TOTAL: &str = "sourcegull_batches_bulk_jobs_total";
/// Counter of changeset syncs, by `outcome` (`ok`, `error`).
pub const BATCHES_CHANGESET_SYNCS_TOTAL: &str = "sourcegull_batches_changeset_syncs_total";
/// Counter of changesets updated from webhooks, by `event` (`merge_request`, `pipeline`).
pub const BATCHES_CHANGESET_WEBHOOK_UPDATES_TOTAL: &str =
    "sourcegull_batches_changeset_webhook_updates_total";
/// Counter of changesets deleted by the retention janitor.
pub const BATCHES_CHANGESETS_PURGED_TOTAL: &str = "sourcegull_batches_changesets_purged_total";
/// Counter of reconciled changesets, by `outcome` (`ok`, `scheduled`, `retry`, `failed`).
pub const BATCHES_CHANGESETS_RECONCILED_TOTAL: &str =
    "sourcegull_batches_changesets_reconciled_total";
/// Counter of changesets released from `scheduled`.
pub const BATCHES_CHANGESETS_RELEASED_TOTAL: &str = "sourcegull_batches_changesets_released_total";

// blobstore
/// Blob API request latency in seconds (until the response headers), by method.
pub const BLOB_REQUEST_DURATION_SECONDS: &str = "sourcegull_blob_request_duration_seconds";
/// Blob API requests served, by method and status.
pub const BLOB_REQUESTS_TOTAL: &str = "sourcegull_blob_requests_total";

// codeintel-policies
/// Counter of uploads deleted because their commit is gone.
pub const CODEINTEL_RETENTION_UNKNOWN_COMMITS_TOTAL: &str =
    "sourcegull_codeintel_retention_unknown_commits_total";
/// Counter of uploads marked expired.
pub const CODEINTEL_RETENTION_UPLOADS_EXPIRED_TOTAL: &str =
    "sourcegull_codeintel_retention_uploads_expired_total";
/// Counter of uploads checked for retention.
pub const CODEINTEL_RETENTION_UPLOADS_SCANNED_TOTAL: &str =
    "sourcegull_codeintel_retention_uploads_scanned_total";

// codeintel-scip
/// Counter of orphaned blobs the janitor deleted.
pub const CODEINTEL_JANITOR_ORPHANED_BLOBS_DELETED_TOTAL: &str =
    "sourcegull_codeintel_janitor_orphaned_blobs_deleted_total";
/// Counter of uploads the janitor deleted, by reason of the deletion pass.
pub const CODEINTEL_JANITOR_UPLOADS_DELETED_TOTAL: &str =
    "sourcegull_codeintel_janitor_uploads_deleted_total";
/// Counter of index bytes processed (as uploaded, usually compressed).
pub const CODEINTEL_UPLOAD_PROCESSED_BYTES_TOTAL: &str =
    "sourcegull_codeintel_upload_processed_bytes_total";
/// Histogram of processing durations in seconds, by outcome.
pub const CODEINTEL_UPLOAD_PROCESSING_SECONDS: &str =
    "sourcegull_codeintel_upload_processing_seconds";

// declarative
/// Counter of changes made by the files, by `kind` and `action`.
pub const DECLARED_CONFIG_CHANGES_TOTAL: &str = "sourcegull_declared_config_changes_total";
/// Gauge per source (`resources`, `global_settings`): 1 while its file cannot be applied.
pub const DECLARED_CONFIG_ERRORS: &str = "sourcegull_declared_config_errors";

// http-api
/// Request latency in seconds, by method and route.
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "sourcegull_http_request_duration_seconds";
/// Requests served, by method, route and status.
pub const HTTP_REQUESTS_TOTAL: &str = "sourcegull_http_requests_total";

// insights
/// Counter of repositories whose value was carried forward without a search.
pub const INSIGHTS_CARRIED_FORWARD_TOTAL: &str = "sourcegull_insights_carried_forward_total";
/// Counter of node calls, by `job` (`backfill`, `record`).
pub const INSIGHTS_COUNT_CALLS_TOTAL: &str = "sourcegull_insights_count_calls_total";

// monitors
/// Counter of deliveries, by `kind` (`email`, `webhook`, `slack_webhook`,
/// `mattermost_webhook`, `telegram`) and `outcome`.
pub const CODE_MONITOR_DELIVERIES_TOTAL: &str = "sourcegull_code_monitor_deliveries_total";
/// Counter of trigger jobs queued, by `trigger` (`event`, `sweep`).
pub const CODE_MONITOR_TRIGGERS_TOTAL: &str = "sourcegull_code_monitor_triggers_total";

// observability
/// Histogram of operation durations in seconds, labelled `op`.
pub const OP_DURATION_SECONDS: &str = "sourcegull_op_duration_seconds";
/// Counter of completed operations, labelled `op` and `outcome` (`ok` | `error`).
pub const OP_TOTAL: &str = "sourcegull_op_total";

// oobmigration
/// Counter of failed batches (label `id`).
pub const OOB_MIGRATION_ERRORS_TOTAL: &str = "sourcegull_oob_migration_errors_total";
/// Gauge of each migration's progress (label `id`).
pub const OOB_MIGRATION_PROGRESS: &str = "sourcegull_oob_migration_progress";

// outbound-webhooks
/// Counter of delivery attempts, by `outcome` (`ok`, `error`).
pub const OUTBOUND_WEBHOOK_DELIVERIES_TOTAL: &str = "sourcegull_outbound_webhook_deliveries_total";
/// Counter of deleted delivery logs.
pub const OUTBOUND_WEBHOOK_LOGS_DELETED_TOTAL: &str =
    "sourcegull_outbound_webhook_logs_deleted_total";

// ranking
/// Duration of the last ranking run.
pub const CODEINTEL_RANKING_DURATION_SECONDS: &str =
    "sourcegull_codeintel_ranking_duration_seconds";
/// Counter of completed ranking runs.
pub const CODEINTEL_RANKING_RUNS_TOTAL: &str = "sourcegull_codeintel_ranking_runs_total";

// sourcegull (the worker's collector and the repo nodes)
/// Failing repositories whose latest failure is an index job (`source="index_job"`) or an
/// upload (`source="upload"`).
pub const CODEINTEL_COVERAGE_FAILING_REPOS: &str = "sourcegull_codeintel_coverage_failing_repos";
/// Repositories by precise index coverage state (`up_to_date`, `stale`, `failing`, `none`,
/// `not_targeted`; see `sourcegull_codeintel_policies::store::CoverageStore`).
pub const CODEINTEL_COVERAGE_REPOS: &str = "sourcegull_codeintel_coverage_repos";
/// Connections of this process's pool by state (`in_use`, `idle`, `max`), labelled `pool`.
pub const DB_POOL_CONNECTIONS: &str = "sourcegull_db_pool_connections";
/// Jobs the active executors report running.
pub const EXECUTOR_RUNNING_JOBS: &str = "sourcegull_executor_running_jobs";
/// Executors that sent a heartbeat within the collector's active window.
pub const EXECUTORS_ACTIVE: &str = "sourcegull_executors_active";
/// Time to build one repository's text index, in seconds.
pub const INDEX_BUILD_DURATION_SECONDS: &str = "sourcegull_index_build_duration_seconds";
/// Repositories waiting in this repo node's index queue.
pub const INDEX_QUEUE_LENGTH: &str = "sourcegull_index_queue_length";
/// Repositories by text index status (`indexed`, `not_indexed`).
pub const INDEX_REPOS: &str = "sourcegull_index_repos";
/// Repositories whose last index build failed.
pub const INDEX_REPOS_ERRORED: &str = "sourcegull_index_repos_errored";
/// Index updates by `outcome`.
pub const INDEX_UPDATES_TOTAL: &str = "sourcegull_index_updates_total";
/// Connections to Sourcegull's database by state (from `pg_stat_activity`).
pub const POSTGRES_CONNECTIONS: &str = "sourcegull_postgres_connections";
/// Size of Sourcegull's database in bytes.
pub const POSTGRES_DATABASE_SIZE_BYTES: &str = "sourcegull_postgres_database_size_bytes";
/// The server's `max_connections`.
pub const POSTGRES_MAX_CONNECTIONS: &str = "sourcegull_postgres_max_connections";
/// Age in seconds of the oldest open transaction on Sourcegull's database.
pub const POSTGRES_OLDEST_TRANSACTION_SECONDS: &str =
    "sourcegull_postgres_oldest_transaction_seconds";
/// 1 when the last collection reached Postgres, else 0.
pub const POSTGRES_UP: &str = "sourcegull_postgres_up";
/// Jobs per queue table and state (`queued`, `processing`, `errored`, `failed`).
pub const QUEUE_JOBS: &str = "sourcegull_queue_jobs";
/// Age in seconds of the oldest queued job per queue table (0 when none is queued).
pub const QUEUE_OLDEST_QUEUED_SECONDS: &str = "sourcegull_queue_oldest_queued_seconds";
/// Repositories this repo node found corrupt.
pub const REPO_CORRUPT_TOTAL: &str = "sourcegull_repo_corrupt_total";
/// Free bytes on the repo node's data disk.
pub const REPO_DISK_FREE_BYTES: &str = "sourcegull_repo_disk_free_bytes";
/// Size in bytes of the repo node's data disk.
pub const REPO_DISK_TOTAL_BYTES: &str = "sourcegull_repo_disk_total_bytes";
/// Pack bytes received by new owners.
pub const REPO_MOVE_BYTES_TOTAL: &str = "sourcegull_repo_move_bytes_total";
/// Time from claiming a move to its flip.
pub const REPO_MOVE_DURATION_SECONDS: &str = "sourcegull_repo_move_duration_seconds";
/// Old copies removed after a move (or found unneeded).
pub const REPO_MOVE_OLD_COPIES_REMOVED_TOTAL: &str =
    "sourcegull_repo_move_old_copies_removed_total";
/// Repository moves between repo nodes by state (`queued`, `copying`, `flipped`, `failed`;
/// rebalancing).
pub const REPO_MOVES: &str = "sourcegull_repo_moves";
/// Moves by outcome (`flipped`, `reassigned`, `cancelled`, `retry`, `failed`).
pub const REPO_MOVES_TOTAL: &str = "sourcegull_repo_moves_total";
/// Fetches the repo node's scheduler enqueued.
pub const REPO_SCHEDULED_FETCHES_TOTAL: &str = "sourcegull_repo_scheduled_fetches_total";
/// Time of one repository clone or fetch in seconds, labelled `kind`.
pub const REPO_UPDATE_DURATION_SECONDS: &str = "sourcegull_repo_update_duration_seconds";
/// Repository clones and fetches by `kind` and `outcome`.
pub const REPO_UPDATES_TOTAL: &str = "sourcegull_repo_updates_total";
/// Repositories (not deleted) by clone status (`not_cloned`, `cloning`, `cloned`).
pub const REPOS: &str = "sourcegull_repos";
/// Repositories marked corrupt by their repo node.
pub const REPOS_CORRUPTED: &str = "sourcegull_repos_corrupted";
/// Repositories whose last fetch failed.
pub const REPOS_FETCH_ERRORS: &str = "sourcegull_repos_fetch_errors";
/// Counter of expired sessions deleted.
pub const SESSIONS_EXPIRED_DELETED_TOTAL: &str = "sourcegull_sessions_expired_deleted_total";
/// Gauge with the version of the loaded site config.
pub const SITE_CONFIG_VERSION: &str = "sourcegull_site_config_version";

/// Every metric name above.
pub const ALL: &[&str] = &[
    EVENT_LOGS_DAYS_ROLLED_UP_TOTAL,
    EVENT_LOGS_PRUNED_TOTAL,
    AUTOINDEX_COMMITS_CONSIDERED_TOTAL,
    AUTOINDEX_JOBS_QUEUED_TOTAL,
    AUTOINDEX_RATE_LIMITED_TOTAL,
    BATCHES_BULK_JOBS_TOTAL,
    BATCHES_CHANGESET_SYNCS_TOTAL,
    BATCHES_CHANGESET_WEBHOOK_UPDATES_TOTAL,
    BATCHES_CHANGESETS_PURGED_TOTAL,
    BATCHES_CHANGESETS_RECONCILED_TOTAL,
    BATCHES_CHANGESETS_RELEASED_TOTAL,
    BLOB_REQUEST_DURATION_SECONDS,
    BLOB_REQUESTS_TOTAL,
    CODEINTEL_RETENTION_UNKNOWN_COMMITS_TOTAL,
    CODEINTEL_RETENTION_UPLOADS_EXPIRED_TOTAL,
    CODEINTEL_RETENTION_UPLOADS_SCANNED_TOTAL,
    CODEINTEL_JANITOR_ORPHANED_BLOBS_DELETED_TOTAL,
    CODEINTEL_JANITOR_UPLOADS_DELETED_TOTAL,
    CODEINTEL_UPLOAD_PROCESSED_BYTES_TOTAL,
    CODEINTEL_UPLOAD_PROCESSING_SECONDS,
    DECLARED_CONFIG_CHANGES_TOTAL,
    DECLARED_CONFIG_ERRORS,
    HTTP_REQUEST_DURATION_SECONDS,
    HTTP_REQUESTS_TOTAL,
    INSIGHTS_CARRIED_FORWARD_TOTAL,
    INSIGHTS_COUNT_CALLS_TOTAL,
    CODE_MONITOR_DELIVERIES_TOTAL,
    CODE_MONITOR_TRIGGERS_TOTAL,
    OP_DURATION_SECONDS,
    OP_TOTAL,
    OOB_MIGRATION_ERRORS_TOTAL,
    OOB_MIGRATION_PROGRESS,
    OUTBOUND_WEBHOOK_DELIVERIES_TOTAL,
    OUTBOUND_WEBHOOK_LOGS_DELETED_TOTAL,
    CODEINTEL_RANKING_DURATION_SECONDS,
    CODEINTEL_RANKING_RUNS_TOTAL,
    CODEINTEL_COVERAGE_FAILING_REPOS,
    CODEINTEL_COVERAGE_REPOS,
    DB_POOL_CONNECTIONS,
    EXECUTOR_RUNNING_JOBS,
    EXECUTORS_ACTIVE,
    INDEX_BUILD_DURATION_SECONDS,
    INDEX_QUEUE_LENGTH,
    INDEX_REPOS,
    INDEX_REPOS_ERRORED,
    INDEX_UPDATES_TOTAL,
    POSTGRES_CONNECTIONS,
    POSTGRES_DATABASE_SIZE_BYTES,
    POSTGRES_MAX_CONNECTIONS,
    POSTGRES_OLDEST_TRANSACTION_SECONDS,
    POSTGRES_UP,
    QUEUE_JOBS,
    QUEUE_OLDEST_QUEUED_SECONDS,
    REPO_CORRUPT_TOTAL,
    REPO_DISK_FREE_BYTES,
    REPO_DISK_TOTAL_BYTES,
    REPO_MOVE_BYTES_TOTAL,
    REPO_MOVE_DURATION_SECONDS,
    REPO_MOVE_OLD_COPIES_REMOVED_TOTAL,
    REPO_MOVES,
    REPO_MOVES_TOTAL,
    REPO_SCHEDULED_FETCHES_TOTAL,
    REPO_UPDATE_DURATION_SECONDS,
    REPO_UPDATES_TOTAL,
    REPOS,
    REPOS_CORRUPTED,
    REPOS_FETCH_ERRORS,
    SESSIONS_EXPIRED_DELETED_TOTAL,
    SITE_CONFIG_VERSION,
];

#[cfg(test)]
mod tests {
    use super::ALL;

    #[test]
    fn names_are_unique_and_prefixed() {
        let mut seen = std::collections::BTreeSet::new();
        for name in ALL {
            assert!(name.starts_with("sourcegull_"), "{name}");
            assert!(seen.insert(name), "{name} is listed twice");
        }
    }
}
