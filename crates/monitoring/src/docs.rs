//! Markdown docs: `alerts.md` (every alert with next steps) and `dashboards.md` (every panel).

use std::fmt::Write as _;

use crate::{Dashboard, GENERATED_NOTICE, Observable, alert_description, alert_name};

fn observables(d: &Dashboard) -> impl Iterator<Item = &Observable> {
    d.groups.iter().flat_map(|g| g.rows.iter().flatten())
}

/// `alerts.md`.
pub fn alerts(dashboards: &[Dashboard]) -> String {
    let mut out = format!(
        "# Alerts\n\n<!-- {GENERATED_NOTICE} -->\n\n\
         Every alert Sourcegull's Prometheus rules can fire, with what to do about it. The rules are \
         generated from `crates/monitoring` into `deploy/docker/prometheus/sourcegull-alerts.yml` \
         (in the Compose bundle, to load into your Prometheus) and the Helm chart's \
         `PrometheusRule` (`metrics.prometheusRule.enabled=true`); [README.md](README.md) shows \
         how to set them up. See [dashboards.md](dashboards.md) for the panels \
         they watch.\n\n\
         Each alert is named `sourcegull_<service>_<observable>` and carries the labels `severity` \
         (`warning`: needs a look soon; `critical`: users are affected or will be shortly), \
         `service`, `owner` and `sourcegull_alert` (the anchor below). A warning and a critical rule \
         share a name; inhibit the warning while the critical one fires. Alertmanager routing \
         is up to your Prometheus setup.\n\n"
    );
    for d in dashboards {
        for o in observables(d).filter(|o| o.alerting()) {
            let name = alert_name(d, o);
            let _ = writeln!(out, "## {name}\n");
            let _ = writeln!(out, "**{}: {}**\n", d.name, o.description);
            out.push_str("**Descriptions**\n\n");
            for (severity, t) in o.alerts() {
                let _ = writeln!(
                    out,
                    "- _{}_: {}",
                    severity.as_str(),
                    alert_description(d, o, &t)
                );
            }
            if !o.interpretation.is_empty() {
                let _ = write!(out, "\n{}\n", o.interpretation);
            }
            out.push_str("\n**Next steps**\n\n");
            for step in o.next_steps {
                let _ = writeln!(out, "- {step}");
            }
            let _ = writeln!(
                out,
                "- Panel: [{}](dashboards.md#{name}) on the {} dashboard.",
                o.description, d.title
            );
            out.push_str("\n**Query**\n\n```promql\n");
            out.push_str(o.query.trim());
            out.push_str("\n```\n\n");
            let _ = writeln!(out, "Owner: {}\n", o.owner.as_str());
        }
    }
    trim_end(out)
}

/// `dashboards.md`.
pub fn dashboards(dashboards: &[Dashboard]) -> String {
    let mut out = format!(
        "# Dashboards\n\n<!-- {GENERATED_NOTICE} -->\n\n\
         Sourcegull's Grafana dashboards, one per service. They are generated from \
         `crates/monitoring` into `deploy/docker/grafana/dashboards/` (in the Compose bundle, to \
         import into your Grafana) and the Helm chart's dashboard ConfigMaps \
         (`metrics.grafanaDashboards.enabled=true`); [README.md](README.md) shows how to set them \
         up. Panels with alerts link to \
         [alerts.md](alerts.md).\n\n"
    );
    for d in dashboards {
        let _ = writeln!(out, "- [{}](#{})", d.title, anchor(d.title));
    }
    out.push('\n');
    for d in dashboards {
        let _ = writeln!(out, "## {}\n", d.title);
        let _ = writeln!(
            out,
            "{}\n\nGrafana uid: `sourcegull-{}`.\n",
            d.description, d.name
        );
        for g in &d.groups {
            let _ = writeln!(
                out,
                "### {}: {}{}\n",
                d.name,
                g.title,
                if g.hidden { " (collapsed)" } else { "" }
            );
            for o in g.rows.iter().flatten() {
                let name = alert_name(d, o);
                let _ = writeln!(out, "#### {name}\n");
                let _ = writeln!(out, "**{}**\n", capitalize(o.description));
                if !o.interpretation.is_empty() {
                    let _ = writeln!(out, "{}\n", o.interpretation);
                }
                if o.alerting() {
                    for (severity, t) in o.alerts() {
                        let _ = writeln!(
                            out,
                            "- Alert ({}): [{}](alerts.md#{name})",
                            severity.as_str(),
                            alert_description(d, o, &t)
                        );
                    }
                    out.push('\n');
                }
                out.push_str("```promql\n");
                out.push_str(o.query.trim());
                out.push_str("\n```\n\n");
                let _ = writeln!(out, "Owner: {}\n", o.owner.as_str());
            }
        }
    }
    trim_end(out)
}

fn trim_end(mut s: String) -> String {
    let len = s.trim_end().len();
    s.truncate(len);
    s.push('\n');
    s
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// GitHub's heading anchor: lowercase, spaces to `-`, punctuation other than `-`/`_` dropped.
fn anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definitions;

    #[test]
    fn every_alert_is_documented_with_next_steps() {
        let dashboards = definitions::dashboards();
        let doc = alerts(&dashboards);
        for d in &dashboards {
            for o in observables(d).filter(|o| o.alerting()) {
                let heading = format!("\n## {}\n", alert_name(d, o));
                let at = doc
                    .find(&heading)
                    .unwrap_or_else(|| panic!("{heading} missing"));
                let section = &doc[at + heading.len()..];
                let section = &section[..section.find("\n## ").unwrap_or(section.len())];
                assert!(section.contains("**Next steps**"), "{heading}");
                for step in o.next_steps {
                    assert!(section.contains(step), "{heading}: {step}");
                }
            }
        }
    }

    #[test]
    fn every_panel_is_documented() {
        let dashboards = definitions::dashboards();
        let doc = super::dashboards(&dashboards);
        for d in &dashboards {
            for o in observables(d) {
                assert!(doc.contains(&format!("\n#### {}\n", alert_name(d, o))));
            }
        }
        assert_eq!(anchor("Sourcegull: web"), "sourcegull-web");
    }
}
