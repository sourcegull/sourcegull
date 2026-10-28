//! Grafana dashboard JSON (schema version 39; works with Grafana 10 and later).
//!
//! Each [`Group`](crate::Group) becomes a row panel; collapsed groups carry their panels
//! inside the row. Panels use the `datasource` template variable, so the dashboards work with
//! whatever Prometheus data source the Grafana instance has.

use serde_json::{Value, json};

use crate::{ALERTS_DOC_URL, Comparison, Dashboard, GENERATED_NOTICE, Observable, alert_name};

/// Grafana uid of a dashboard.
pub fn uid(d: &Dashboard) -> String {
    format!("sourcegull-{}", d.name)
}

/// File name of a dashboard.
pub fn file_name(d: &Dashboard) -> String {
    format!("{}.json", uid(d))
}

const PANEL_HEIGHT: u32 = 8;
const GRID_WIDTH: u32 = 24;

fn datasource() -> Value {
    json!({ "type": "prometheus", "uid": "${datasource}" })
}

/// Renders `d` as pretty-printed dashboard JSON with a trailing newline.
pub fn render(d: &Dashboard) -> String {
    let mut panels = Vec::new();
    let mut next_id = 1u32;
    let mut y = 0u32;
    for group in &d.groups {
        let row_id = next_id;
        next_id += 1;
        let row_y = y;
        y += 1;
        let mut children = Vec::new();
        for row in &group.rows {
            let width = GRID_WIDTH / row.len() as u32;
            for (i, o) in row.iter().enumerate() {
                children.push(panel(d, o, next_id, i as u32 * width, y, width));
                next_id += 1;
            }
            y += PANEL_HEIGHT;
        }
        let mut row_panel = json!({
            "id": row_id,
            "type": "row",
            "title": group.title,
            "collapsed": group.hidden,
            "gridPos": { "h": 1, "w": GRID_WIDTH, "x": 0, "y": row_y },
            "panels": [],
        });
        if group.hidden {
            // A collapsed row holds its panels; Grafana lays them out when it opens.
            row_panel["panels"] = Value::Array(children);
            y = row_y + 1;
            panels.push(row_panel);
        } else {
            panels.push(row_panel);
            panels.extend(children);
        }
    }
    let dashboard = json!({
        "uid": uid(d),
        "title": d.title,
        "description": format!("{} {}", d.description, GENERATED_NOTICE),
        "tags": ["sourcegull"],
        "editable": true,
        "graphTooltip": 1,
        "schemaVersion": 39,
        "version": 1,
        "time": { "from": "now-6h", "to": "now" },
        "refresh": "1m",
        "timezone": "",
        "links": [{
            "title": "Sourcegull dashboards",
            "type": "dashboards",
            "tags": ["sourcegull"],
            "asDropdown": true,
            "includeVars": true,
            "keepTime": true,
        }],
        "templating": { "list": [{
            "name": "datasource",
            "label": "Data source",
            "type": "datasource",
            "query": "prometheus",
            "current": {},
            "hide": 0,
            "refresh": 1,
            "regex": "",
        }]},
        "annotations": { "list": [] },
        "panels": panels,
    });
    let mut out = serde_json::to_string_pretty(&dashboard).expect("dashboard JSON");
    out.push('\n');
    out
}

fn panel(d: &Dashboard, o: &Observable, id: u32, x: u32, y: u32, w: u32) -> Value {
    let mut description = if o.interpretation.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", o.interpretation)
    };
    for (severity, t) in o.alerts() {
        description.push_str(&format!(
            "Alerts ({}): {}.\n",
            severity.as_str(),
            crate::alert_description(d, o, &t)
        ));
    }
    if o.alerting() {
        description.push_str(&format!(
            "\nNext steps: {ALERTS_DOC_URL}#{}",
            alert_name(d, o)
        ));
    }
    json!({
        "id": id,
        "type": "timeseries",
        "title": o.description,
        "description": description.trim_end(),
        "datasource": datasource(),
        "gridPos": { "h": PANEL_HEIGHT, "w": w, "x": x, "y": y },
        "targets": [{
            "refId": "A",
            "datasource": datasource(),
            "expr": o.query,
            "legendFormat": if o.legend.is_empty() { "__auto" } else { o.legend },
            "range": true,
        }],
        "fieldConfig": {
            "defaults": {
                "unit": o.unit.grafana(),
                "min": 0,
                "custom": {
                    "drawStyle": "line",
                    "lineWidth": 1,
                    "fillOpacity": 10,
                    "showPoints": "never",
                    "spanNulls": true,
                    "thresholdsStyle": { "mode": if o.alerting() { "line" } else { "off" } },
                },
                "thresholds": thresholds(o),
            },
            "overrides": [],
        },
        "options": {
            "legend": { "displayMode": "list", "placement": "bottom", "showLegend": true },
            "tooltip": { "mode": "multi", "sort": "desc" },
        },
    })
}

/// Threshold steps: green in the normal range, orange past warning, red past critical.
fn thresholds(o: &Observable) -> Value {
    let below = [o.warning, o.critical]
        .iter()
        .flatten()
        .any(|t| t.comparison == Comparison::Below);
    let mut steps = Vec::new();
    if below {
        // Steps apply from their value upwards, so the worst level comes first.
        steps.push(
            json!({ "color": if o.critical.is_some() { "red" } else { "orange" }, "value": null }),
        );
        if let (Some(c), Some(w)) = (o.critical, o.warning) {
            steps.push(json!({ "color": "orange", "value": c.value }));
            steps.push(json!({ "color": "green", "value": w.value }));
        } else if let Some(t) = o.critical.or(o.warning) {
            steps.push(json!({ "color": "green", "value": t.value }));
        }
    } else {
        steps.push(json!({ "color": "green", "value": null }));
        if let Some(w) = o.warning {
            steps.push(json!({ "color": "orange", "value": w.value }));
        }
        if let Some(c) = o.critical {
            steps.push(json!({ "color": "red", "value": c.value }));
        }
    }
    json!({ "mode": "absolute", "steps": steps })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Group, Owner, Threshold, Unit};

    fn dashboard() -> Dashboard {
        Dashboard {
            name: "test",
            title: "Test",
            description: "A test.",
            groups: vec![
                Group::new(
                    "Shown",
                    vec![vec![
                        Observable::new("a", "first", "up", Owner::Platform)
                            .unit(Unit::Ratio)
                            .warning(Threshold::above(0.1))
                            .critical(Threshold::above(0.5))
                            .next_steps(&["Look."]),
                        Observable::new("b", "second", "up", Owner::Platform)
                            .critical(Threshold::below(1.0))
                            .next_steps(&["Look."]),
                    ]],
                ),
                Group::hidden(
                    "Hidden",
                    vec![vec![
                        Observable::new("c", "third", "up", Owner::Platform)
                            .interpretation("Read it."),
                    ]],
                ),
            ],
        }
    }

    #[test]
    fn lays_out_rows_and_panels() {
        let v: Value = serde_json::from_str(&render(&dashboard())).expect("json");
        assert_eq!(v["uid"], "sourcegull-test");
        let panels = v["panels"].as_array().expect("panels");
        // Row, two panels, collapsed row.
        assert_eq!(panels.len(), 4);
        assert_eq!(panels[0]["type"], "row");
        assert_eq!(panels[1]["gridPos"]["w"], 12);
        assert_eq!(panels[2]["gridPos"]["x"], 12);
        assert_eq!(panels[1]["gridPos"]["y"], 1);
        assert_eq!(panels[3]["collapsed"], true);
        assert_eq!(panels[3]["gridPos"]["y"], 9);
        assert_eq!(panels[3]["panels"][0]["title"], "third");
        let ids: std::collections::HashSet<_> = panels
            .iter()
            .chain(panels[3]["panels"].as_array().expect("nested"))
            .map(|p| p["id"].as_u64())
            .collect();
        assert_eq!(ids.len(), 5, "unique panel ids");
    }

    #[test]
    fn thresholds_follow_the_comparison() {
        let d = dashboard();
        let above = thresholds(&d.groups[0].rows[0][0]);
        assert_eq!(
            above["steps"][1],
            json!({ "color": "orange", "value": 0.1 })
        );
        assert_eq!(above["steps"][2], json!({ "color": "red", "value": 0.5 }));
        let below = thresholds(&d.groups[0].rows[0][1]);
        assert_eq!(below["steps"][0]["color"], "red");
        assert_eq!(below["steps"][1], json!({ "color": "green", "value": 1.0 }));
        let v: Value = serde_json::from_str(&render(&d)).expect("json");
        let desc = v["panels"][1]["description"].as_str().expect("description");
        assert!(desc.contains("test: first above 10%"), "{desc}");
        assert!(desc.contains("alerts.md#test_a"), "{desc}");
    }
}
