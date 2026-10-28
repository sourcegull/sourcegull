//! The `blob` role: the built-in blob store (SCIP uploads, bulk change files, search job
//! results). Empty when the blob store is S3.

use super::{instances_up, quantile, ratio};
use sourcegull_types::metric_names;

use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

const REQUESTS: &str = metric_names::BLOB_REQUESTS_TOTAL;

pub fn dashboard() -> Dashboard {
    Dashboard {
        name: "blob",
        title: "Sourcegull: blob",
        description: "The blob role stores SCIP uploads, bulk change workspace files and search job results on its volume and serves them to the other roles. With an S3-compatible store (`blobstore.mode=s3`) there is no blob role and this dashboard stays empty.",
        groups: vec![
            Group::new(
                "Requests",
                vec![vec![
                    Observable::new(
                        "request_rate",
                        "blob requests per second by method and status",
                        format!(r#"sum by (method, status) (rate({REQUESTS}{{role="blob"}}[5m]))"#),
                        Owner::Platform,
                    )
                    .legend("{{method}} {{status}}")
                    .unit(Unit::PerSecond)
                    .interpretation("`PUT` writes, `GET`/`HEAD` reads, `DELETE` removals. 404 on reads is normal for missing keys."),
                    Observable::new(
                        "error_ratio",
                        "share of blob requests answered with 5xx",
                        ratio(REQUESTS, r#"role="blob""#, r#"status=~"5..""#, "", "10m"),
                        Owner::Platform,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.01).for_minutes(10))
                    .critical(Threshold::above(0.1).for_minutes(10))
                    .next_steps(&[
                        "Read the blob role's logs for `blob store request failed`.",
                        "A full or read-only volume is the usual cause: check the free space of the blob volume (Helm `blobstore.persistence.size`).",
                    ]),
                    Observable::new(
                        "p90_latency",
                        "90th percentile blob request duration by method",
                        quantile(0.9, "sourcegull_blob_request_duration_seconds", r#"role="blob""#, "method", "5m"),
                        Owner::Platform,
                    )
                    .legend("{{method}}")
                    .unit(Unit::Seconds)
                    .interpretation("Until the response starts: reads stream their body afterwards, writes include receiving the upload."),
                ]],
            ),
            Group::new(
                "Instances",
                vec![vec![instances_up(
                    "blob",
                    &[
                        "Check the blob pod or container (`kubectl get pods -l app.kubernetes.io/component=blob`, `docker compose ps blob`) and its logs.",
                        "While it is down, SCIP uploads, bulk change file uploads and search job downloads fail.",
                    ],
                )]],
            ),
        ],
    }
}
