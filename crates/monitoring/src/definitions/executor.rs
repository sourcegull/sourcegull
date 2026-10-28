//! Executors: the executor queues, the executors' heartbeats and the executor API on `web`.
//!
//! Executor processes expose no metrics endpoint; everything here is seen from the server side
//! (queue tables, heartbeats, the web role's request metrics).

use super::{EXECUTOR_QUEUES, ratio};
use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

const API: &str = r#"role="web",route=~"/.executors/.*""#;

pub fn dashboard() -> Dashboard {
    let queues = format!(r#"queue=~"{EXECUTOR_QUEUES}""#);
    Dashboard {
        name: "executor",
        title: "Sourcegull: executor",
        description: "Executors take auto-indexing jobs (`codeintel_index_jobs`) and server-side bulk change spec executions (`batch_spec_workspace_execution_jobs`) from the web role's executor API and run them in containers or Kubernetes jobs. Executor processes expose no metrics; this dashboard shows their queues, heartbeats and API traffic.",
        groups: vec![
            Group::new(
                "Executors",
                vec![vec![
                    Observable::new(
                        "executors_active",
                        "executors sending heartbeats",
                        "max(sourcegull_executors_active)",
                        Owner::Platform,
                    )
                    .interpretation("Executors that sent a heartbeat in the last minute (they send one every 5 s). Site admin > Executors lists them."),
                    Observable::new(
                        "running_jobs",
                        "jobs running on executors",
                        "max(sourcegull_executor_running_jobs)",
                        Owner::Platform,
                    )
                    .interpretation("Jobs the active executors report running, across both queues."),
                    Observable::new(
                        "jobs_without_executors",
                        "queued executor jobs while no executor is active",
                        format!(
                            "sum(max by (queue) (sourcegull_queue_jobs{{{queues},state=\"queued\"}}))\n* on() (max(sourcegull_executors_active) == bool 0)"
                        ),
                        Owner::Platform,
                    )
                    .warning(Threshold::above(0.0).for_minutes(15))
                    .interpretation("The number of queued executor jobs when no executor has sent a heartbeat for a minute, else 0.")
                    .next_steps(&[
                        "Start executors: Compose `docker compose --profile executors up -d`, Helm `executor.enabled=true`.",
                        "If executors run, check their logs: a wrong `SOURCEGULL_EXECUTOR_TOKEN` (must equal `executors.accessToken` in the site config) or an unreachable `SOURCEGULL_EXECUTOR_FRONTEND_URL` stops heartbeats.",
                        "Without executors, disable auto-indexing (Site admin > Code graph policies) and server-side bulk changes so jobs stop piling up.",
                    ]),
                ]],
            ),
            Group::new(
                "Executor queues",
                vec![vec![
                    Observable::new(
                        "queue_depth",
                        "queued and running executor jobs by queue",
                        format!(r#"max by (queue, state) (sourcegull_queue_jobs{{{queues},state=~"queued|processing"}})"#),
                        Owner::Platform,
                    )
                    .legend("{{queue}} {{state}}")
                    .interpretation("Jobs waiting for and held by executors."),
                    Observable::new(
                        "queue_oldest_queued",
                        "age of the oldest queued executor job by queue",
                        format!("max by (queue) (sourcegull_queue_oldest_queued_seconds{{{queues}}})"),
                        Owner::Platform,
                    )
                    .legend("{{queue}}")
                    .unit(Unit::Seconds)
                    .warning(Threshold::above(3600.0).for_minutes(30))
                    .next_steps(&[
                        "Executors are busy or missing: compare running jobs with the executors' capacity (`SOURCEGULL_EXECUTOR_MAX_JOBS` / Helm `executor.maxConcurrentJobs` per executor).",
                        "Add executors (Helm `executor.replicaCount`) or raise their concurrency if the nodes have room.",
                        "Executors only take queues listed in `SOURCEGULL_EXECUTOR_QUEUES` (Helm `executor.queues`); check the queue is in the list.",
                    ]),
                    Observable::new(
                        "queue_failed",
                        "failed and errored executor jobs kept by queue",
                        format!(r#"max by (queue, state) (sourcegull_queue_jobs{{{queues},state=~"failed|errored"}})"#),
                        Owner::Platform,
                    )
                    .legend("{{queue}} {{state}}")
                    .interpretation("Failed auto-indexing jobs are shown per repository under Code graph > Index jobs; failed bulk change spec workspaces on the bulk change spec's execution page."),
                ]],
            ),
            Group::new(
                "Executor API",
                vec![vec![
                    Observable::new(
                        "api_requests",
                        "executor API requests per second by route",
                        format!("sum by (route) (rate(sourcegull_http_requests_total{{{API}}}[5m]))"),
                        Owner::Platform,
                    )
                    .legend("{{route}}")
                    .unit(Unit::PerSecond)
                    .interpretation("Heartbeats, dequeues, log updates, git and file downloads from executors."),
                    Observable::new(
                        "api_5xx_ratio",
                        "share of executor API requests answered with 5xx",
                        ratio("sourcegull_http_requests_total", API, r#"status=~"5..""#, "", "15m"),
                        Owner::Platform,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.05).for_minutes(15))
                    .next_steps(&[
                        "Read the web logs for errors on `/.executors/` routes; git requests are served from the repo nodes, so check the repo dashboard too.",
                        "Upload failures of batch step results can come from the blob store: check the blob dashboard.",
                    ]),
                ]],
            ),
        ],
    }
}
