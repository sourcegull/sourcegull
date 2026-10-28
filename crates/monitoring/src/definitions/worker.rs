//! The `worker` role: job queues and background jobs of every feature.

use super::{EXECUTOR_QUEUES, instances_up, quantile, ratio};
use sourcegull_types::metric_names;

use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

/// Queues the workers process: everything but the executor queues and the repo nodes' queue.
fn worker_queues() -> String {
    format!(r#"queue!~"{EXECUTOR_QUEUES}|repo_update_jobs""#)
}

const UPLOADS: &str = metric_names::CODEINTEL_UPLOAD_PROCESSING_SECONDS;

pub fn dashboard() -> Dashboard {
    let queues = worker_queues();
    Dashboard {
        name: "worker",
        title: "Sourcegull: worker",
        description: "Workers run the job queues (code host and permission syncs, SCIP uploads, bulk changes, search jobs, watches, trends, outbound webhooks) and the singleton background jobs.",
        groups: vec![
            Group::new(
                "Job queues",
                vec![
                    vec![
                        Observable::new(
                            "queue_depth",
                            "queued jobs by queue",
                            format!(r#"max by (queue) (sourcegull_queue_jobs{{state="queued",{queues}}})"#),
                            Owner::Platform,
                        )
                        .legend("{{queue}}")
                        .interpretation("Jobs waiting for a worker, per queue table. Site admin > Background jobs shows the same counts with the newest failures."),
                        Observable::new(
                            "queue_processing",
                            "running jobs by queue",
                            format!(r#"max by (queue) (sourcegull_queue_jobs{{state="processing",{queues}}})"#),
                            Owner::Platform,
                        )
                        .legend("{{queue}}")
                        .interpretation("Jobs a worker holds right now. Stalled jobs (no heartbeat) are returned to the queue by the resetters."),
                    ],
                    vec![
                        Observable::new(
                            "queue_oldest_queued",
                            "age of the oldest queued job by queue",
                            format!(r#"max by (queue) (sourcegull_queue_oldest_queued_seconds{{{queues}}})"#),
                            Owner::Platform,
                        )
                        .legend("{{queue}}")
                        .unit(Unit::Seconds)
                        .warning(Threshold::above(21600.0).for_minutes(30))
                        .next_steps(&[
                            "Check that the workers are up (instances panel) and their logs for errors from the queue's handler.",
                            "Site admin > Background jobs shows the queue's oldest jobs and recent failures; a single job type failing and retrying can hold up the rest.",
                            "Queues that do much work per job (trend backfills, search jobs) can lag after large changes; add worker replicas or raise the feature's concurrency setting (e.g. `insights.query.worker.concurrency`, `batchChanges.reconcilerConcurrency`).",
                        ]),
                        Observable::new(
                            "queue_failed",
                            "failed and errored jobs kept by queue",
                            format!(r#"max by (queue) (sourcegull_queue_jobs{{state=~"failed|errored",{queues}}})"#),
                            Owner::Platform,
                        )
                        .legend("{{queue}}")
                        .interpretation("`errored` jobs are retried, `failed` ones are not. Growth means a job type keeps failing; Site admin > Background jobs lists the messages and can retry them."),
                    ],
                ],
            ),
            Group::new(
                "Code intelligence",
                vec![
                    vec![
                        Observable::new(
                            "upload_processing_rate",
                            "SCIP uploads processed per second by outcome",
                            format!("sum by (outcome) (rate({UPLOADS}_count[5m]))"),
                            Owner::CodeIntel,
                        )
                        .legend("{{outcome}}")
                        .unit(Unit::PerSecond)
                        .interpretation("`completed`, `errored` (retried) and `failed` (given up) upload processing runs."),
                        Observable::new(
                            "upload_processing_failure_ratio",
                            "share of SCIP upload processing runs failing",
                            ratio(&format!("{UPLOADS}_count"), "", r#"outcome!="completed""#, "", "1h"),
                            Owner::CodeIntel,
                        )
                        .unit(Unit::Ratio)
                        .warning(Threshold::above(0.25).for_minutes(30))
                        .next_steps(&[
                            "The repository's Code graph > Uploads page shows each failed upload's error.",
                            "`unknown commit` errors mean the repo node has not fetched the uploaded commit yet; they are retried. Check the repo dashboard if they persist.",
                            "Check the worker logs for `processing an upload failed` and the blob store (blob dashboard) for errors reading upload files.",
                        ]),
                        Observable::new(
                            "upload_processing_p90_duration",
                            "90th percentile SCIP upload processing duration",
                            quantile(0.9, UPLOADS, "", "", "30m"),
                            Owner::CodeIntel,
                        )
                        .unit(Unit::Seconds)
                        .interpretation("Histogram buckets end at 60 s, so larger uploads show as 60 s."),
                        Observable::new(
                            "upload_processed_bytes",
                            "SCIP upload bytes processed per second",
                            "sum(rate(sourcegull_codeintel_upload_processed_bytes_total[5m]))",
                            Owner::CodeIntel,
                        )
                        .unit(Unit::BytesPerSecond)
                        .interpretation("Size of the SCIP indexes turned into code graph data."),
                    ],
                    vec![
                        Observable::new(
                            "autoindex_commits_considered",
                            "commits considered for auto-indexing per second",
                            "sum by (trigger) (rate(sourcegull_autoindex_commits_considered_total[5m]))",
                            Owner::CodeIntel,
                        )
                        .legend("{{trigger}}")
                        .unit(Unit::PerSecond)
                        .interpretation("Commits the auto-indexing scheduler looked at, on ref updates (`event`) and in its sweep."),
                        Observable::new(
                            "autoindex_jobs_queued",
                            "auto-indexing jobs queued per second",
                            "sum(rate(sourcegull_autoindex_jobs_queued_total[5m]))",
                            Owner::CodeIntel,
                        )
                        .unit(Unit::PerSecond)
                        .interpretation("Index jobs added to the `codeintel_index_jobs` executor queue (see the executor dashboard)."),
                        Observable::new(
                            "autoindex_rate_limited",
                            "auto-indexing passes skipped by the rate limit per second",
                            "sum(rate(sourcegull_autoindex_rate_limited_total[5m]))",
                            Owner::CodeIntel,
                        )
                        .unit(Unit::PerSecond)
                        .interpretation("Repositories skipped because they were indexed too recently."),
                    ],
                    vec![
                        Observable::new(
                            "codeintel_coverage_repos",
                            "repositories by precise index coverage",
                            "max by (state) (sourcegull_codeintel_coverage_repos)",
                            Owner::CodeIntel,
                        )
                        .legend("{{state}}")
                        .interpretation("`up_to_date`: a completed index serves the default branch; `stale`: the newest index is not at the default branch tip and older than 7 days; `failing`: the latest index job or upload failed; `none`: an indexing policy applies but nothing is indexed; `not_targeted`: no indexing policy applies. Site admin > Index coverage lists the repositories. Refreshed every 5 minutes."),
                        Observable::new(
                            "codeintel_failing_repos_growth",
                            "growth in repositories whose precise indexing fails, over 6 hours",
                            r#"max(sourcegull_codeintel_coverage_repos{state="failing"}) - max(sourcegull_codeintel_coverage_repos{state="failing"} offset 6h)"#,
                            Owner::CodeIntel,
                        )
                        .warning(Threshold::above(0.0).for_minutes(60))
                        .next_steps(&[
                            "Site admin > Index coverage, filtered to Failing, lists the repositories with the latest failure message; each links to the repository's Code graph page with the job's logs.",
                            "Many repositories failing at once usually means the executors or an indexer image broke: check the executor dashboard and an index job's logs.",
                            "Code navigation in a failing repository answers from its last completed index or from search, so results can be incomplete until indexing succeeds again.",
                        ]),
                        Observable::new(
                            "codeintel_failing_repos_by_source",
                            "failing repositories by where the latest failure happened",
                            "max by (source) (sourcegull_codeintel_coverage_failing_repos)",
                            Owner::CodeIntel,
                        )
                        .legend("{{source}}")
                        .interpretation("`index_job`: an auto-indexing job failed on an executor; `upload`: processing an uploaded index failed."),
                    ],
                ],
            ),
            Group::hidden(
                "Code intelligence data retention",
                vec![vec![
                    Observable::new(
                        "codeintel_uploads_deleted",
                        "SCIP uploads deleted per hour by reason",
                        "sum by (reason) (increase(sourcegull_codeintel_janitor_uploads_deleted_total[1h]))",
                        Owner::CodeIntel,
                    )
                    .legend("{{reason}}")
                    .interpretation("Uploads removed by the janitor: `expired` by data retention policies, `abandoned` (never finished uploading) and `repository deleted`."),
                    Observable::new(
                        "codeintel_orphaned_blobs_deleted",
                        "orphaned upload files deleted per hour",
                        "sum(increase(sourcegull_codeintel_janitor_orphaned_blobs_deleted_total[1h]))",
                        Owner::CodeIntel,
                    )
                    .interpretation("Blob store files without an upload, removed by the janitor."),
                    Observable::new(
                        "codeintel_retention_expired",
                        "uploads expired by data retention policies per hour",
                        "sum(increase(sourcegull_codeintel_retention_uploads_expired_total[1h]))",
                        Owner::CodeIntel,
                    )
                    .interpretation("Uploads the expirer marked expired; the janitor deletes them later. Uploads on commits the repo node does not know are skipped until it does."),
                    Observable::new(
                        "codeintel_ranking_duration",
                        "duration of the last ranking run",
                        "max(sourcegull_codeintel_ranking_duration_seconds)",
                        Owner::CodeIntel,
                    )
                    .unit(Unit::Seconds)
                    .interpretation("How long the last PageRank run over the code graph took."),
                ]],
            ),
            Group::new(
                "Bulk changes",
                vec![vec![
                    Observable::new(
                        "reconciler_outcomes",
                        "changesets reconciled per second by outcome",
                        "sum by (outcome) (rate(sourcegull_batches_changesets_reconciled_total[5m]))",
                        Owner::Batches,
                    )
                    .legend("{{outcome}}")
                    .unit(Unit::PerSecond)
                    .interpretation("`ok`: the changeset matches its spec on the code host; `scheduled`: waiting for the rollout window; `retry`/`failed`: an error."),
                    Observable::new(
                        "reconciler_failure_ratio",
                        "share of changeset reconciliations failing",
                        ratio("sourcegull_batches_changesets_reconciled_total", "", r#"outcome="failed""#, "", "30m"),
                        Owner::Batches,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.2).for_minutes(30))
                    .next_steps(&[
                        "The bulk change's page lists the failed changesets with the code host's error.",
                        "Most failures are code host permissions: the bulk change's credential (user or site credential) needs `api` scope and Developer access to the project.",
                        "Check the code host's rate limits and status; the outbound requests log (Site admin > Outbound requests) shows recent calls when `outboundRequestLogLimit` is set.",
                    ]),
                    Observable::new(
                        "changeset_sync_error_ratio",
                        "share of changeset syncs failing",
                        ratio("sourcegull_batches_changeset_syncs_total", "", r#"outcome="error""#, "", "1h"),
                        Owner::Batches,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.5).for_minutes(60))
                    .next_steps(&[
                        "The syncer reads merge request state from GitLab; check the code host connection (Site admin > Code host connections > Test) and its token.",
                        "Worker logs show the failing changesets and errors.",
                    ]),
                    Observable::new(
                        "bulk_operations",
                        "bulk operations per second by type and outcome",
                        "sum by (type, outcome) (rate(sourcegull_batches_bulk_jobs_total[5m]))",
                        Owner::Batches,
                    )
                    .legend("{{type}} {{outcome}}")
                    .unit(Unit::PerSecond)
                    .interpretation("Bulk actions on changesets: comment, detach, reenqueue, merge, close, publish."),
                ]],
            ),
            Group::new(
                "Watches, Trends and outbound webhooks",
                vec![vec![
                    Observable::new(
                        "code_monitor_triggers",
                        "watch searches queued per second",
                        "sum by (trigger) (rate(sourcegull_code_monitor_triggers_total[5m]))",
                        Owner::CodeInsights,
                    )
                    .legend("{{trigger}}")
                    .unit(Unit::PerSecond)
                    .interpretation("Trigger searches queued on pushes and by the evaluator's sweep."),
                    Observable::new(
                        "code_monitor_delivery_error_ratio",
                        "share of watch notifications failing by kind",
                        ratio("sourcegull_code_monitor_deliveries_total", "", r#"outcome="error""#, "kind", "1h"),
                        Owner::CodeInsights,
                    )
                    .legend("{{kind}}")
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.5).for_minutes(30))
                    .next_steps(&[
                        "Email: check the SMTP settings (`email.smtp` and `email.address` in the site config) and the worker logs for SMTP errors.",
                        "Slack, Mattermost and webhooks: the receiving URL rejects or does not answer; the watch's page shows the last errors. Fix or remove the action.",
                        "Telegram: the Bot API refuses the message (wrong bot token, or the bot is not in the chat) or is unreachable from the worker; the watch's page shows the Bot API's answer.",
                    ]),
                    Observable::new(
                        "insights_search_calls",
                        "Trend searches per second by job",
                        "sum by (job) (rate(sourcegull_insights_count_calls_total[5m]))",
                        Owner::CodeInsights,
                    )
                    .legend("{{job}}")
                    .unit(Unit::PerSecond)
                    .interpretation("Searches run by trend backfills and recordings; limited by `insights.query.worker.rateLimit`."),
                    Observable::new(
                        "outbound_webhook_error_ratio",
                        "share of outbound webhook deliveries failing",
                        ratio("sourcegull_outbound_webhook_deliveries_total", "", r#"outcome="error""#, "", "1h"),
                        Owner::Platform,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.5).for_minutes(30))
                    .next_steps(&[
                        "Site admin > Outbound webhooks shows each webhook's recent deliveries with status and error.",
                        "Fix or remove the receiving endpoint; failed deliveries are retried with backoff.",
                    ]),
                ]],
            ),
            Group::new(
                "Out-of-band migrations",
                vec![vec![
                    Observable::new(
                        "oob_migration_progress",
                        "out-of-band migration progress",
                        "max by (id) (sourcegull_oob_migration_progress)",
                        Owner::Platform,
                    )
                    .legend("{{id}}")
                    .unit(Unit::Ratio)
                    .interpretation("Progress of background data migrations, e.g. re-encryption after a key rotation. Site admin > Migrations shows the same."),
                    Observable::new(
                        "oob_migration_errors",
                        "out-of-band migration errors in the last hour",
                        "sum by (id) (increase(sourcegull_oob_migration_errors_total[1h]))",
                        Owner::Platform,
                    )
                    .legend("{{id}}")
                    .warning(Threshold::above(0.0))
                    .next_steps(&[
                        "Site admin > Migrations lists the migration's last errors.",
                        "Re-encryption errors usually mean a key is missing from the key file (`SOURCEGULL_KEYS_FILE`) or `SOURCEGULL_KEYS`: keep old keys there until the migration completes.",
                        "Upgrades refuse to pass a version whose out-of-band migrations are incomplete, so fix the cause before upgrading.",
                    ]),
                ]],
            ),
            Group::hidden(
                "Housekeeping",
                vec![vec![
                    Observable::new(
                        "sessions_expired_deleted",
                        "expired sessions deleted per hour",
                        "sum(increase(sourcegull_sessions_expired_deleted_total[1h]))",
                        Owner::Platform,
                    )
                    .interpretation("Removed by the session janitor every 10 minutes."),
                    Observable::new(
                        "changesets_purged",
                        "changesets deleted by retention per hour",
                        "sum(increase(sourcegull_batches_changesets_purged_total[1h]))",
                        Owner::Batches,
                    )
                    .interpretation("Detached changesets older than `batchChanges.changesetsRetention`."),
                    Observable::new(
                        "changesets_released",
                        "changesets released by the rollout window scheduler per hour",
                        "sum(increase(sourcegull_batches_changesets_released_total[1h]))",
                        Owner::Batches,
                    )
                    .interpretation("Changesets published as `batchChanges.rolloutWindows` allows."),
                    Observable::new(
                        "outbound_webhook_logs_deleted",
                        "outbound webhook logs deleted per hour",
                        "sum(increase(sourcegull_outbound_webhook_logs_deleted_total[1h]))",
                        Owner::Platform,
                    )
                    .interpretation("Delivery logs past their retention, removed by the janitor."),
                ]],
            ),
            Group::new(
                "Instances",
                vec![vec![instances_up(
                    "worker",
                    &[
                        "Check the worker containers or pods (`docker compose ps worker`, `kubectl get pods -l app.kubernetes.io/component=worker`) and their logs.",
                        "A worker refuses to start while migrations are pending.",
                        "While no worker runs, queues grow and singleton jobs (syncs, janitors, schedulers) stop; they resume when a worker is back.",
                    ],
                )]],
            ),
        ],
    }
}
