//! The `web` role: UI, GraphQL, HTTP APIs, streaming search, webhooks.

use super::{instances_up, quantile, ratio};
use sourcegull_types::metric_names;

use crate::{Dashboard, Group, Observable, Owner, Threshold, Unit};

const REQUESTS: &str = metric_names::HTTP_REQUESTS_TOTAL;
const DURATION: &str = metric_names::HTTP_REQUEST_DURATION_SECONDS;
/// Requests of users and API clients: the web role without the probes and scrapes.
const WEB: &str = r#"role="web",route!~"/metrics|/healthz|/readyz""#;
/// Long-lived or bulk routes, left out of the latency panel: streams, uploads, the executor API.
const LONG: &str = r#"role="web",route!~"/metrics|/healthz|/readyz|/.api/search/stream|/.api/compute/stream|/.api/blame/.*|/.api/scip/upload|/.api/files/.*|/.executors/.*""#;
const WEBHOOKS: &str = r#"role="web",route=~"/.api/gitlab-webhooks|/.api/webhooks/.*""#;

pub fn dashboard() -> Dashboard {
    Dashboard {
        name: "web",
        title: "Sourcegull: web",
        description: "The web role serves the UI, the GraphQL and HTTP APIs, streaming search, code host webhooks and the executor API.",
        groups: vec![
            Group::new(
                "HTTP requests",
                vec![
                    vec![
                        Observable::new(
                            "http_request_rate",
                            "requests per second by route",
                            format!("sum by (route) (rate({REQUESTS}{{{WEB}}}[5m]))"),
                            Owner::Platform,
                        )
                        .legend("{{route}}")
                        .unit(Unit::PerSecond)
                        .interpretation("Requests to the public listener by matched route pattern (`fallback` is the UI and raw files). Health checks and scrapes are left out."),
                        Observable::new(
                            "http_5xx_ratio",
                            "share of requests answered with 5xx",
                            ratio(REQUESTS, WEB, r#"status=~"5..""#, "", "5m"),
                            Owner::Platform,
                        )
                        .unit(Unit::Ratio)
                        .warning(Threshold::above(0.02).for_minutes(5))
                        .critical(Threshold::above(0.1).for_minutes(10))
                        .next_steps(&[
                            "Find the failing routes: in Grafana, run `sum by (route, status) (rate(sourcegull_http_requests_total{role=\"web\",status=~\"5..\"}[5m]))`.",
                            "Read the web logs for `error` entries around that time (`docker compose logs web`, `kubectl logs -l app.kubernetes.io/component=web`). Failed responses carry an `x-request-id` that appears in the log lines.",
                            "Many 502/503 from GraphQL or search often mean Postgres or the repo nodes are unreachable: check the Postgres and repo dashboards.",
                        ]),
                    ],
                    vec![
                        Observable::new(
                            "http_p90_latency",
                            "90th percentile request duration by route",
                            quantile(0.9, DURATION, LONG, "route", "5m"),
                            Owner::Platform,
                        )
                        .legend("{{route}}")
                        .unit(Unit::Seconds)
                        .warning(Threshold::above(5.0).for_minutes(15))
                        .next_steps(&[
                            "Check which routes are slow on this panel; GraphQL (`/.api/graphql`) has its own panel below.",
                            "Slow GraphQL requests and searches are logged above `observability.logSlowGraphQLRequests` / `observability.logSlowSearches` and listed under Site admin > Slow requests.",
                            "Check Postgres (connection saturation, long transactions) and CPU usage of the web instances; add web replicas if they are CPU-bound.",
                        ]),
                    ],
                ],
            ),
            Group::new(
                "Search and GraphQL",
                vec![vec![
                    Observable::new(
                        "search_stream_p90_duration",
                        "90th percentile streaming search duration",
                        quantile(0.9, DURATION, r#"role="web",route="/.api/search/stream""#, "", "5m"),
                        Owner::Search,
                    )
                    .unit(Unit::Seconds)
                    .warning(Threshold::above(20.0).for_minutes(15))
                    .interpretation("Time from the request until the search stream ends, as users see it. Histogram buckets end at 60 s.")
                    .next_steps(&[
                        "Check the repo dashboard: index queue length and index errors (unindexed repositories are searched without the index, which is slower), and the repo node instances.",
                        "Look for expensive queries under Site admin > Slow requests (`observability.logSlowSearches`).",
                        "Check CPU and memory of the repo nodes; the index's hot set should fit in RAM (see the Helm sizing guide).",
                    ]),
                    Observable::new(
                        "search_stream_requests",
                        "streaming searches per second by status",
                        format!(r#"sum by (status) (rate({REQUESTS}{{role="web",route="/.api/search/stream"}}[5m]))"#),
                        Owner::Search,
                    )
                    .legend("{{status}}")
                    .unit(Unit::PerSecond)
                    .interpretation("Search requests from the UI, the API and `gull`. Errors inside a stream are sent as events and still count as 200."),
                    Observable::new(
                        "graphql_p90_duration",
                        "90th percentile GraphQL request duration",
                        quantile(0.9, DURATION, r#"role="web",route="/.api/graphql""#, "", "5m"),
                        Owner::Platform,
                    )
                    .unit(Unit::Seconds)
                    .warning(Threshold::above(5.0).for_minutes(15))
                    .next_steps(&[
                        "Find the slow operations under Site admin > Slow requests (enable `observability.logSlowGraphQLRequests`).",
                        "Check the Postgres dashboard for connection saturation and long transactions.",
                    ]),
                ]],
            ),
            Group::new(
                "Code host webhooks",
                vec![vec![
                    Observable::new(
                        "webhook_requests",
                        "code host webhook deliveries per second by status",
                        format!("sum by (route, status) (rate({REQUESTS}{{{WEBHOOKS}}}[5m]))"),
                        Owner::Source,
                    )
                    .legend("{{route}} {{status}}")
                    .unit(Unit::PerSecond)
                    .interpretation("GitLab webhook deliveries (repository pushes, merge requests, pipelines) by response status."),
                    Observable::new(
                        "webhook_rejection_ratio",
                        "share of code host webhooks rejected",
                        ratio(REQUESTS, WEBHOOKS, r#"status!~"2..""#, "", "30m"),
                        Owner::Source,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.5).for_minutes(30))
                    .next_steps(&[
                        "401/403: the secret token in GitLab does not match the webhook's secret. Compare them on the code host connection page (Site admin > Code host connections > Webhooks) and in GitLab (project or group > Settings > Webhooks).",
                        "404: the webhook was deleted in Sourcegull but GitLab still sends to it; remove it in GitLab.",
                        "Recent deliveries and their errors are listed under Site admin > Webhook logs.",
                    ]),
                ]],
            ),
            Group::new(
                "Instances",
                vec![vec![
                    instances_up(
                        "web",
                        &[
                            "Check the web containers or pods (`docker compose ps web`, `kubectl get pods -l app.kubernetes.io/component=web`) and their logs for a crash or failed start.",
                            "A web instance refuses to start while database migrations are pending: run `sourcegull migrate up` (the Helm chart's migrate job does this on upgrade).",
                            "If the instance runs but cannot be scraped, check that Prometheus reaches port 6060 (NetworkPolicy, Compose network).",
                        ],
                    ),
                    Observable::new(
                        "site_config_version",
                        "site configuration version loaded by each instance",
                        "max by (role, instance) (sourcegull_site_config_version)",
                        Owner::Platform,
                    )
                    .legend("{{role}} {{instance}}")
                    .interpretation("The site config version each process runs with. With the config in the database, all instances converge on the same version within seconds of a change; a lasting difference means an instance cannot reload it (its logs say why). With `SOURCEGULL_SITE_CONFIG_FILE`, versions count reloads per process and may differ."),
                ]],
            ),
        ],
    }
}
