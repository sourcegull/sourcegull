//! The `repo` role: repository clones and fetches, the text search index and disk.

use super::{instances_up, quantile, ratio};
use sourcegull_types::metric_names;

use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

const UPDATES: &str = metric_names::REPO_UPDATES_TOTAL;
const INDEX_UPDATES: &str = metric_names::INDEX_UPDATES_TOTAL;
const MOVES: &str = metric_names::REPO_MOVES_TOTAL;

pub fn dashboard() -> Dashboard {
    Dashboard {
        name: "repo",
        title: "Sourcegull: repo",
        description: "Repo nodes clone and fetch repositories from the code hosts, build the text search index and serve git data and searches to the other roles over gRPC.",
        groups: vec![
            Group::new(
                "Disk",
                vec![vec![
                    Observable::new(
                        "disk_free_ratio",
                        "free disk space on the data volume",
                        r#"min by (instance) (sourcegull_repo_disk_free_bytes{role="repo"} / sourcegull_repo_disk_total_bytes{role="repo"})"#,
                        Owner::Source,
                    )
                    .legend("{{instance}}")
                    .unit(Unit::Ratio)
                    .interpretation("Measured by the repo node janitor (2 minutes after start, then hourly). The node pauses clones when free space is below both SOURCEGULL_REPO_PAUSE_CLONES_FREE_PERCENT (default 10%) of the volume and SOURCEGULL_REPO_PAUSE_CLONES_FREE_GIB (default 20 GiB); Site admin > Repo nodes shows the threshold.")
                    .warning(Threshold::below(0.15).for_minutes(10))
                    .critical(Threshold::below(0.05).for_minutes(5))
                    .next_steps(&[
                        "Grow the node's volume (Helm: `repo.persistence.size` on a storage class that allows expansion; Compose: the `repo-0-data` volume's disk).",
                        "Or add a repo node so repositories spread over more disks (Helm: `repo.replicaCount`).",
                        "Check Site admin > Repo nodes and Repositories for unexpectedly large repositories; excluding them in the code host connection frees their space after the next sync.",
                        "The repo node janitor removes repositories deleted from Sourcegull and maintains the object stores; its errors appear in the repo node's logs.",
                    ]),
                    Observable::new(
                        "disk_free_bytes",
                        "free bytes on the data volume",
                        r#"min by (instance) (sourcegull_repo_disk_free_bytes{role="repo"})"#,
                        Owner::Source,
                    )
                    .legend("{{instance}}")
                    .unit(Unit::Bytes)
                    .interpretation("Free space for clones, the search index and caches, measured hourly. A repo node needs about 1.5 times the size of its repositories."),
                ]],
            ),
            Group::new(
                "Clones and fetches",
                vec![
                    vec![
                        Observable::new(
                            "repo_update_rate",
                            "clones and fetches per second by kind and outcome",
                            format!(r#"sum by (kind, outcome) (rate({UPDATES}{{role="repo"}}[5m]))"#),
                            Owner::Source,
                        )
                        .legend("{{kind}} {{outcome}}")
                        .unit(Unit::PerSecond)
                        .interpretation("Repository updates finished by the repo nodes: `clone` (first copy), `fetch` (scheduled or webhook-triggered update) and `reclone`."),
                        Observable::new(
                            "repo_update_error_ratio",
                            "share of clones and fetches failing",
                            ratio(UPDATES, r#"role="repo""#, r#"outcome="error""#, "kind", "30m"),
                            Owner::Source,
                        )
                        .legend("{{kind}}")
                        .unit(Unit::Ratio)
                        .warning(Threshold::above(0.2).for_minutes(30))
                        .critical(Threshold::above(0.8).for_minutes(30))
                        .next_steps(&[
                            "Site admin > Repositories lists the failing repositories with their last error (filter: Failed fetch).",
                            "If most updates fail, the code host is probably unreachable or the connection's token expired or lost access: test the connection under Site admin > Code host connections.",
                            "Check the repo node logs for `repo update failed` entries; corrupt clones are re-cloned from Site admin > Repositories.",
                        ]),
                    ],
                    vec![
                        Observable::new(
                            "repo_update_p90_duration",
                            "90th percentile clone and fetch duration by kind",
                            quantile(0.9, "sourcegull_repo_update_duration_seconds", r#"role="repo""#, "kind", "15m"),
                            Owner::Source,
                        )
                        .legend("{{kind}}")
                        .unit(Unit::Seconds)
                        .interpretation("Histogram buckets end at 60 s, so long clones show as 60 s."),
                        Observable::new(
                            "update_queue_oldest",
                            "age of the oldest queued clone or fetch",
                            r#"max(sourcegull_queue_oldest_queued_seconds{queue="repo_update_jobs"})"#,
                            Owner::Source,
                        )
                        .unit(Unit::Seconds)
                        .warning(Threshold::above(7200.0).for_minutes(30))
                        .next_steps(&[
                            "Check that every repo node is up (instances panel) and processing: `sourcegull_repo_updates_total` should grow.",
                            "A large backlog after adding a code host connection drains by itself; to speed it up raise `gitMaxConcurrentClones` in the site config (read when a repo node starts) and `SOURCEGULL_REPO_MAX_CONCURRENT_FETCHES` on the repo nodes, or add repo nodes.",
                            "Queued and failed jobs are listed under Site admin > Background jobs (`repo_update_jobs`).",
                        ]),
                        Observable::new(
                            "update_queue_depth",
                            "queued and running clones and fetches",
                            r#"max by (state) (sourcegull_queue_jobs{queue="repo_update_jobs",state=~"queued|processing"})"#,
                            Owner::Source,
                        )
                        .legend("{{state}}")
                        .interpretation("Jobs in the `repo_update_jobs` queue. Scheduled fetches and webhook pushes add jobs; one queued job per repository at most."),
                    ],
                ],
            ),
            Group::new(
                "Repositories",
                vec![vec![
                    Observable::new(
                        "repos_by_clone_status",
                        "repositories by clone status",
                        "max by (clone_status) (sourcegull_repos)",
                        Owner::Source,
                    )
                    .legend("{{clone_status}}")
                    .interpretation("Repositories known to Sourcegull (not deleted). `not_cloned` falls as the repo nodes clone new repositories."),
                    Observable::new(
                        "repos_fetch_errors",
                        "repositories whose last update failed",
                        "max(sourcegull_repos_fetch_errors)",
                        Owner::Source,
                    )
                    .interpretation("Repositories with a last error recorded by their repo node; Site admin > Repositories lists them (filter: Failed fetch)."),
                    Observable::new(
                        "repos_corrupted",
                        "repositories marked corrupt",
                        "max(sourcegull_repos_corrupted)",
                        Owner::Source,
                    )
                    .warning(Threshold::above(0.0).for_minutes(5))
                    .next_steps(&[
                        "Re-clone the repositories from Site admin > Repositories (filter: Corrupted).",
                        "Repeated corruption on one node points to its disk or file system: check the node's kernel and volume logs.",
                    ]),
                ]],
            ),
            Group::new(
                "Search index",
                vec![
                    vec![
                        Observable::new(
                            "index_queue_length",
                            "repositories waiting to be indexed",
                            r#"max by (instance) (sourcegull_index_queue_length{role="repo"})"#,
                            Owner::Search,
                        )
                        .legend("{{instance}}")
                        .warning(Threshold::above(500.0).for_minutes(60))
                        .next_steps(&[
                            "After a large import the queue drains by itself; check the build rate on the index updates panel.",
                            "If it does not shrink, check the repo node logs for `indexing failed` and its CPU usage; raise the node's CPU or add repo nodes.",
                            "Searches over unindexed repositories still work but are slower.",
                        ]),
                        Observable::new(
                            "index_update_rate",
                            "index updates per second by outcome",
                            format!(r#"sum by (outcome) (rate({INDEX_UPDATES}{{role="repo"}}[5m]))"#),
                            Owner::Search,
                        )
                        .legend("{{outcome}}")
                        .unit(Unit::PerSecond)
                        .interpretation("`built`: new shards written; `unchanged`: nothing to do; `removed`: index dropped; `error`: the build failed."),
                        Observable::new(
                            "index_error_ratio",
                            "share of index builds failing",
                            ratio(INDEX_UPDATES, r#"role="repo""#, r#"outcome="error""#, "", "30m"),
                            Owner::Search,
                        )
                        .unit(Unit::Ratio)
                        .warning(Threshold::above(0.2).for_minutes(30))
                        .next_steps(&[
                            "Site admin > Repositories shows the index status and error of each repository (filter: Not indexed).",
                            "Check the repo node logs for `indexing failed`; disk full (see the disk panels) is a common cause.",
                        ]),
                    ],
                    vec![
                        Observable::new(
                            "index_build_p90_duration",
                            "90th percentile index build duration",
                            quantile(0.9, "sourcegull_index_build_duration_seconds", r#"role="repo""#, "", "30m"),
                            Owner::Search,
                        )
                        .unit(Unit::Seconds)
                        .interpretation("Time to build one repository's shards. Histogram buckets end at 60 s."),
                        Observable::new(
                            "index_repos",
                            "repositories by index status",
                            "max by (status) (sourcegull_index_repos)",
                            Owner::Search,
                        )
                        .legend("{{status}}")
                        .interpretation("Repositories with a text index (`indexed`) and without one yet (`not_indexed`)."),
                        Observable::new(
                            "index_repos_errored",
                            "repositories whose last index build failed",
                            "max(sourcegull_index_repos_errored)",
                            Owner::Search,
                        )
                        .interpretation("Cleared by the next successful build. Site admin > Repositories shows the error per repository."),
                    ],
                ],
            ),
            Group::new(
                "Rebalancing",
                vec![vec![
                    Observable::new(
                        "repo_moves",
                        "repository moves between repo nodes by state",
                        "max by (state) (sourcegull_repo_moves)",
                        Owner::Source,
                    )
                    .legend("{{state}}")
                    .interpretation("After repo nodes are added or removed, the repositories whose node changes are copied to their new node (`queued`, `copying`), switched over (`flipped`, the old copy is kept for a few minutes) and then removed from the old node. Nothing moves until every repo node runs with the same node list."),
                    Observable::new(
                        "repo_move_rate",
                        "repository moves finished per second by outcome",
                        format!(r#"sum by (outcome) (rate({MOVES}{{role="repo"}}[5m]))"#),
                        Owner::Source,
                    )
                    .legend("{{outcome}}")
                    .unit(Unit::PerSecond)
                    .interpretation("`flipped`: copied and switched over; `reassigned`: the old node was gone, cloned from the code host instead; `retry`: an attempt failed and is retried; `failed`: gave up; `cancelled`: no longer needed."),
                    Observable::new(
                        "repo_move_throughput",
                        "bytes copied between repo nodes per second",
                        r#"sum by (instance) (rate(sourcegull_repo_move_bytes_total{role="repo"}[5m]))"#,
                        Owner::Source,
                    )
                    .legend("{{instance}}")
                    .unit(Unit::BytesPerSecond)
                    .interpretation("Pack data received by the repositories' new nodes."),
                    Observable::new(
                        "repo_moves_failed",
                        "repository moves that gave up",
                        r#"max(sourcegull_repo_moves{state="failed"})"#,
                        Owner::Source,
                    )
                    .warning(Threshold::above(0.0).for_minutes(30))
                    .next_steps(&[
                        "Site admin > Repo nodes lists the failed moves with their errors; the repositories stay on their old node and keep working.",
                        "Common causes: the old node is unreachable from the new one (gRPC port 3178), or the new node's disk is full.",
                        "Fix the cause, then use Retry failed moves on that page.",
                    ]),
                ]],
            ),
            Group::new(
                "Instances",
                vec![vec![instances_up(
                    "repo",
                    &[
                        "Check the repo pods or containers (`kubectl get pods -l app.kubernetes.io/component=repo`, `docker compose ps repo-0`) and their logs.",
                        "A repo node refuses to start while migrations are pending, and when its data volume is not writable.",
                        "While a node is down, searches skip its repositories (results say so) and its clones and fetches wait.",
                    ],
                )]],
            ),
        ],
    }
}
