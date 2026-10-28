//! The dashboards, one per service.
//!
//! Metrics come from three places: the RED metrics every process records for its HTTP routes
//! (`sourcegull_http_*`, public and internal listeners), the counters and histograms of the domain
//! crates (`sourcegull_repo_*`, `sourcegull_index_*`, `sourcegull_codeintel_*`, …), and the gauges the
//! worker reads from Postgres (`sourcegull_queue_*`, `sourcegull_executors_*`, `sourcegull_postgres_*`,
//! see `sourcegull::metrics_collector`). Scrape configs add a `role` label (`web`, `repo`,
//! `worker`, `blob`) to every series; gauges the workers collect are read with `max` because
//! every worker reports the same values.

mod blob;
mod executor;
mod postgres;
mod repo;
mod web;
mod worker;

use crate::{Dashboard, Observable, Owner, Threshold};

/// Every dashboard, in the order of the docs.
pub fn dashboards() -> Vec<Dashboard> {
    vec![
        web::dashboard(),
        repo::dashboard(),
        worker::dashboard(),
        executor::dashboard(),
        blob::dashboard(),
        postgres::dashboard(),
    ]
}

/// The executor queues (taken by executors over HTTP, not by workers).
pub(crate) const EXECUTOR_QUEUES: &str = "codeintel_index_jobs|batch_spec_workspace_execution_jobs";

/// `up` per instance of `role`, alerting when an instance stops answering scrapes.
pub(crate) fn instances_up(role: &'static str, next_steps: &'static [&'static str]) -> Observable {
    Observable::new(
        "instance_up",
        "instances answering Prometheus scrapes",
        format!("up{{role=\"{role}\"}}"),
        Owner::Platform,
    )
    .legend("{{instance}}")
    .interpretation(
        "1 while Prometheus can scrape the instance's internal listener (:6060), 0 when it cannot.",
    )
    .critical(Threshold::below(1.0).for_minutes(5))
    .next_steps(next_steps)
}

/// `histogram_quantile(q, sum by (le, <by>) (rate(<metric>_bucket{<selector>}[<window>])))`.
pub(crate) fn quantile(q: f64, metric: &str, selector: &str, by: &str, window: &str) -> String {
    let by = if by.is_empty() {
        "le".to_owned()
    } else {
        format!("le, {by}")
    };
    format!(
        "histogram_quantile({q}, sum by ({by}) (rate({}[{window}])))",
        select(&format!("{metric}_bucket"), selector)
    )
}

/// Share of `metric` (a counter) whose series match `bad`, among all matching `selector`.
///
/// The numerator falls back to `0 * <all>`, so the share reads 0 (not "no data") while there is
/// traffic but nothing bad.
pub(crate) fn ratio(metric: &str, selector: &str, bad: &str, by: &str, window: &str) -> String {
    let by = if by.is_empty() {
        String::new()
    } else {
        format!(" by ({by}) ")
    };
    let sep = if selector.is_empty() { "" } else { "," };
    let all = format!("sum{by}(rate({}[{window}]))", select(metric, selector));
    format!("(sum{by}(rate({metric}{{{selector}{sep}{bad}}}[{window}])) or 0 * {all})\n/ {all}")
}

/// `metric{selector}`, or `metric` for an empty selector (`metric{}` is not valid PromQL).
pub(crate) fn select(metric: &str, selector: &str) -> String {
    if selector.is_empty() {
        metric.to_owned()
    } else {
        format!("{metric}{{{selector}}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_helpers() {
        assert_eq!(
            quantile(0.9, "m", "role=\"web\"", "route", "5m"),
            "histogram_quantile(0.9, sum by (le, route) (rate(m_bucket{role=\"web\"}[5m])))"
        );
        assert_eq!(
            ratio("c", "role=\"web\"", "status=~\"5..\"", "", "5m"),
            "(sum(rate(c{role=\"web\",status=~\"5..\"}[5m])) or 0 * sum(rate(c{role=\"web\"}[5m])))\n/ sum(rate(c{role=\"web\"}[5m]))"
        );
        assert_eq!(
            ratio("c", "", "outcome=\"error\"", "kind", "1h"),
            "(sum by (kind) (rate(c{outcome=\"error\"}[1h])) or 0 * sum by (kind) (rate(c[1h])))\n/ sum by (kind) (rate(c[1h]))"
        );
    }
}
