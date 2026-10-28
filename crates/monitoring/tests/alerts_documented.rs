//! "All alerts documented", checked on the files that ship rather than on the definitions: every
//! alerting rule in any YAML file under `deploy/` (the generated Compose and Helm copies, and
//! anything written by hand) has a section in `docs/observability/alerts.md` with next steps, its
//! `runbook_url` points at that section, and the page documents no alert that no rule fires.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const DOC: &str = "docs/observability/alerts.md";
const RUNBOOK: &str =
    "https://github.com/sourcegull/sourcegull/blob/main/docs/observability/alerts.md#";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One `- alert:` rule: its name and the values of its `sourcegull_alert` label and `runbook_url`
/// annotation.
#[derive(Debug, Default)]
struct Rule {
    file: String,
    alert: String,
    anchor: Option<String>,
    runbook: Option<String>,
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim_matches('\'').to_owned()
}

/// The rules of a Prometheus rule file (or a manifest embedding rules), by line: every
/// `alert:` key starts a rule.
fn rules(file: &str, text: &str) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start().trim_start_matches("- ");
        if let Some(name) = line.strip_prefix("alert:") {
            out.push(Rule {
                file: file.to_owned(),
                alert: unquote(name),
                ..Rule::default()
            });
        } else if let Some(rule) = out.last_mut() {
            if let Some(v) = line.strip_prefix("sourcegull_alert:") {
                rule.anchor = Some(unquote(v));
            } else if let Some(v) = line.strip_prefix("runbook_url:") {
                rule.runbook = Some(unquote(v));
            }
        }
    }
    out
}

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            yaml_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "yml" || e == "yaml" || e == "tpl")
        {
            out.push(path);
        }
    }
}

/// The `## <anchor>` sections of the alerts page.
fn sections(doc: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut parts = doc.split("\n## ").skip(1);
    for part in parts.by_ref() {
        let (heading, body) = part.split_once('\n').unwrap_or((part, ""));
        out.insert(heading.trim().to_owned(), body.to_owned());
    }
    out
}

#[test]
fn every_shipped_alert_is_documented() {
    let root = repo_root();
    let mut files = Vec::new();
    yaml_files(&root.join("deploy"), &mut files);
    let mut all = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read yaml");
        let name = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        all.extend(rules(&name, &text));
    }
    assert!(
        all.len() >= 10,
        "found only {} alerting rules under deploy/",
        all.len()
    );
    for copy in [
        "deploy/docker/prometheus/sourcegull-alerts.yml",
        "deploy/helm/sourcegull/files/monitoring/sourcegull-alerts.yml",
    ] {
        assert!(
            all.iter().any(|r| r.file == copy),
            "no alerting rules in {copy}"
        );
    }

    let doc = std::fs::read_to_string(root.join(DOC)).expect("alerts.md");
    let sections = sections(&doc);
    let mut problems = Vec::new();
    let mut fired = BTreeSet::new();
    for r in &all {
        let id = format!("{} in {}", r.alert, r.file);
        let Some(anchor) = &r.anchor else {
            problems.push(format!("{id}: no sourcegull_alert label"));
            continue;
        };
        fired.insert(anchor.clone());
        if r.alert != format!("sourcegull_{anchor}") {
            problems.push(format!(
                "{id}: name does not match sourcegull_alert {anchor}"
            ));
        }
        if r.runbook.as_deref() != Some(&format!("{RUNBOOK}{anchor}")) {
            problems.push(format!(
                "{id}: runbook_url {:?} is not {RUNBOOK}{anchor}",
                r.runbook
            ));
        }
        match sections.get(anchor) {
            None => problems.push(format!("{id}: no `## {anchor}` section in {DOC}")),
            Some(body) => {
                let steps = body
                    .split_once("**Next steps**")
                    .map(|(_, rest)| rest.split("\n**").next().unwrap_or(""))
                    .unwrap_or("");
                if !steps.lines().any(|l| l.starts_with("- ") && l.len() > 10) {
                    problems.push(format!("{id}: section `{anchor}` has no next steps"));
                }
                if !body.contains("**Query**") {
                    problems.push(format!("{id}: section `{anchor}` has no query"));
                }
            }
        }
    }
    for anchor in sections.keys() {
        if !fired.contains(anchor) {
            problems.push(format!(
                "{DOC} documents `{anchor}`, which no rule under deploy/ fires"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn parses_rules() {
    let text = "groups:\n  - name: g\n    rules:\n      - alert: sourcegull_a_b\n        labels:\n          sourcegull_alert: \"a_b\"\n        annotations:\n          runbook_url: \"x#a_b\"\n      - alert: other\n";
    let r = rules("f", text);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].alert, "sourcegull_a_b");
    assert_eq!(r[0].anchor.as_deref(), Some("a_b"));
    assert_eq!(r[0].runbook.as_deref(), Some("x#a_b"));
    assert!(r[1].anchor.is_none());
}
