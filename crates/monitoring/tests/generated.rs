//! The generated dashboards, rules and docs are checked in (Compose, the Helm chart and
//! `docs/observability/`). Regenerate with
//! `SOURCEGULL_UPDATE_MONITORING=1 cargo test -p sourcegull-monitoring --test integration generated::`
//! (or `sourcegull monitoring generate --repo .`).
//!
//! Also checks that every metric the queries use is in `sourcegull_types::metric_names::ALL`, the
//! names the recording code uses, and that no code names a metric with a bare literal.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sourcegull_monitoring::{Output, definitions};
use sourcegull_types::metric_names;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn generated_files_are_current() {
    let root = repo_root();
    let output = Output::render(&definitions::dashboards());
    if std::env::var_os("SOURCEGULL_UPDATE_MONITORING").is_some() {
        output.write_repo(&root).expect("write generated files");
        return;
    }
    let mut stale = Vec::new();
    for (path, content) in &output.files {
        for repo_path in Output::repo_paths(path) {
            let on_disk = std::fs::read_to_string(root.join(&repo_path)).unwrap_or_default();
            if &on_disk != content {
                stale.push(repo_path);
            }
        }
    }
    for extra in output.unexpected_repo_files(&root) {
        stale.push(format!("{extra} (no longer generated)"));
    }
    assert!(
        stale.is_empty(),
        "stale generated monitoring files: {stale:?}\n\
         run SOURCEGULL_UPDATE_MONITORING=1 cargo test -p sourcegull-monitoring --test integration generated::"
    );
}

/// Metric names in a PromQL query: `sourcegull_*` identifiers and any identifier followed by a
/// label matcher (`up{…}`).
fn metric_names(query: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &query[start..i];
            let in_string = query[..start].matches('"').count() % 2 == 1;
            if !in_string && (ident.starts_with("sourcegull_") || bytes.get(i) == Some(&b'{')) {
                names.insert(ident.to_owned());
            }
        } else {
            i += 1;
        }
    }
    names
}

/// `"sourcegull_…"` string literals that are not metric names.
const NOT_METRICS: &[&str] = &[
    "sourcegull_oauth_state",
    "sourcegull_session",
    "sourcegull_test_installed_total",
];

/// Every `"sourcegull_…"` string literal in the workspace's Rust sources outside this crate and
/// `sourcegull_types::metric_names`, with its file.
fn name_literals() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && !path.ends_with("types/src/metric_names.rs")
            {
                let text = std::fs::read_to_string(&path).expect("read source");
                for literal in text.split('"').skip(1).step_by(2) {
                    if literal.starts_with("sourcegull_")
                        && literal
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        out.push((path.clone(), literal.to_owned()));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(repo_root().join("crates"))
        .expect("crates")
        .flatten()
    {
        if entry.file_name() != "monitoring" {
            walk(&entry.path().join("src"), &mut out);
        }
    }
    out
}

#[test]
fn metrics_are_named_by_constants() {
    let literals = name_literals();
    assert!(
        literals.iter().any(|(_, l)| l == "sourcegull_session"),
        "source scan found nothing"
    );
    let stray: Vec<_> = literals
        .iter()
        .filter(|(_, l)| !NOT_METRICS.contains(&l.as_str()))
        .map(|(p, l)| format!("{}: {l}", p.display()))
        .collect();
    assert!(
        stray.is_empty(),
        "name metrics with a constant in sourcegull_types::metric_names: {stray:?}"
    );
}

/// Metrics Prometheus itself provides.
const PROMETHEUS_METRICS: &[&str] = &["up"];

#[test]
fn queries_use_only_recorded_metrics() {
    let recorded: BTreeSet<&str> = metric_names::ALL.iter().copied().collect();
    let mut unknown = Vec::new();
    for d in definitions::dashboards() {
        for o in d.groups.iter().flat_map(|g| g.rows.iter().flatten()) {
            for name in metric_names(&o.query) {
                let base = ["_bucket", "_count", "_sum"]
                    .iter()
                    .find_map(|s| name.strip_suffix(s))
                    .unwrap_or(&name);
                let known = recorded.contains(name.as_str())
                    || recorded.contains(base)
                    || PROMETHEUS_METRICS.contains(&name.as_str());
                if !known {
                    unknown.push(format!("{}/{}: {name}", d.name, o.name));
                }
            }
        }
    }
    assert!(unknown.is_empty(), "metrics nobody records: {unknown:?}");
}

#[test]
fn metric_name_extraction() {
    let names = metric_names(
        r#"sum by (route) (rate(sourcegull_http_requests_total{role="web",route!~"/sourcegull_x"}[5m])) / up{role="web"}"#,
    );
    assert_eq!(
        names.into_iter().collect::<Vec<_>>(),
        ["sourcegull_http_requests_total", "up"]
    );
}
