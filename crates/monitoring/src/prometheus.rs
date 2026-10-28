//! Prometheus alerting rules: one group per dashboard, one rule per threshold.
//!
//! Each rule is `(<query>) <op> <value>` with `for`, labelled `severity`, `service` and
//! `owner`, and annotated with a summary, the query and a `runbook_url` into alerts.md. A
//! warning and a critical rule of one observable share the alert name, so Alertmanager can
//! inhibit the warning while the critical one fires.
//!
//! The YAML is written by hand (the structure is fixed): strings are JSON-quoted, which YAML
//! reads as double-quoted scalars, and queries are block scalars.

use std::fmt::Write as _;

use crate::{ALERTS_DOC_URL, Dashboard, GENERATED_NOTICE, alert_description, alert_name};

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string")
}

/// `300` → `5m`, `90` → `90s`.
fn prometheus_duration(secs: u32) -> String {
    if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Renders the rules file for `dashboards`.
pub fn render(dashboards: &[Dashboard]) -> String {
    let mut out = format!("# {GENERATED_NOTICE}\n# Alert docs: {ALERTS_DOC_URL}\ngroups:\n");
    for d in dashboards {
        let _ = writeln!(out, "  - name: sourcegull-{}", d.name);
        out.push_str("    rules:\n");
        for o in d.groups.iter().flat_map(|g| g.rows.iter().flatten()) {
            let name = alert_name(d, o);
            for (severity, t) in o.alerts() {
                let _ = writeln!(out, "      - alert: sourcegull_{name}");
                out.push_str("        expr: |-\n");
                // Every line of the query is indented into the block scalar.
                let query = o.query.trim().replace('\n', "\n           ");
                let _ = writeln!(out, "          ({query})");
                let _ = writeln!(out, "            {} {}", t.operator(), t.value);
                if t.for_secs > 0 {
                    let _ = writeln!(out, "        for: {}", prometheus_duration(t.for_secs));
                }
                out.push_str("        labels:\n");
                let _ = writeln!(out, "          severity: {}", severity.as_str());
                let _ = writeln!(out, "          service: {}", d.name);
                let _ = writeln!(out, "          owner: {}", o.owner.as_str());
                let _ = writeln!(out, "          sourcegull_alert: {}", quote(&name));
                out.push_str("        annotations:\n");
                let _ = writeln!(
                    out,
                    "          summary: {}",
                    quote(&format!(
                        "{} ({})",
                        alert_description(d, o, &t),
                        severity.as_str()
                    ))
                );
                let _ = writeln!(
                    out,
                    "          description: {}",
                    quote(&format!(
                        "{}. Current value: {{{{ $value }}}}. See the runbook for next steps.",
                        alert_description(d, o, &t)
                    ))
                );
                let _ = writeln!(
                    out,
                    "          runbook_url: {}",
                    quote(&format!("{ALERTS_DOC_URL}#{name}"))
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Group, Observable, Owner, Threshold, Unit};

    #[test]
    fn renders_a_rule_per_threshold() {
        let d = Dashboard {
            name: "web",
            title: "Web",
            description: "",
            groups: vec![Group::new(
                "g",
                vec![vec![
                    Observable::new(
                        "errors",
                        "error \"ratio\"",
                        "sum(rate(x[5m]))\n/ sum(rate(y[5m]))",
                        Owner::Platform,
                    )
                    .unit(Unit::Ratio)
                    .warning(Threshold::above(0.05).for_minutes(5))
                    .critical(Threshold::above(0.2).for_minutes(10))
                    .next_steps(&["x"]),
                ]],
            )],
        };
        let out = render(&[d]);
        assert!(out.contains("  - name: sourcegull-web\n"), "{out}");
        assert_eq!(
            out.matches("- alert: sourcegull_web_errors\n").count(),
            2,
            "{out}"
        );
        assert!(out.contains("for: 5m"), "{out}");
        assert!(out.contains("severity: critical"), "{out}");
        assert!(
            out.contains("summary: \"web: error \\\"ratio\\\" above 5% for 5m (warning)\""),
            "{out}"
        );
        assert!(out.contains("alerts.md#web_errors\""), "{out}");
        // Multi-line queries stay inside the block scalar.
        assert!(
            out.contains(
                "          (sum(rate(x[5m]))\n           / sum(rate(y[5m])))\n            > 0.05"
            ),
            "{out}"
        );
    }
}
