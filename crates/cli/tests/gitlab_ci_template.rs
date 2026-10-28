//! The GitLab CI template (`deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml`) parses, its
//! `!reference` tags and `needs` point at jobs that exist, every upload job runs under the same
//! rules as the index job it needs, every upload calls `gull code-intel upload`, and the
//! indexer image defaults are the digests auto-indexing uses.

use serde_yaml::Value;

// The auto-indexing defaults, compiled from their source file (it uses only std).
#[allow(dead_code)]
#[path = "../../autoindex/src/indexers.rs"]
mod indexers;

const TEMPLATE: &str = include_str!("../../../deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml");

fn check_references(doc: &Value, v: &Value) {
    match v {
        Value::Tagged(t) => {
            assert_eq!(t.tag.to_string(), "!reference", "unexpected tag {}", t.tag);
            let path: Vec<&str> = t
                .value
                .as_sequence()
                .expect("!reference takes a list")
                .iter()
                .map(|p| p.as_str().expect("!reference path parts are strings"))
                .collect();
            let mut at = doc;
            for part in &path {
                at = at
                    .get(part)
                    .unwrap_or_else(|| panic!("!reference {path:?} points nowhere"));
            }
        }
        Value::Sequence(s) => s.iter().for_each(|v| check_references(doc, v)),
        Value::Mapping(m) => m.values().for_each(|v| check_references(doc, v)),
        _ => {}
    }
}

const MISSING_CLI_IMAGE: &str = "sourcegull-scip-missing-cli-image";

#[test]
fn gitlab_ci_template_is_consistent() {
    let doc: Value = serde_yaml::from_str(TEMPLATE).expect("the template is valid YAML");
    check_references(&doc, &doc);
    let map = doc.as_mapping().unwrap();
    let jobs: Vec<(&str, &Value)> = map
        .iter()
        .map(|(k, v)| (k.as_str().unwrap(), v))
        .filter(|(k, _)| !k.starts_with('.') && *k != "variables")
        .collect();
    let names: Vec<&str> = jobs.iter().map(|(k, _)| *k).collect();
    for language in ["go", "typescript", "java", "python", "rust"] {
        let index = format!("sourcegull-scip-{language}");
        let upload = format!("{index}-upload");
        assert!(names.contains(&index.as_str()), "{index} is missing");
        assert!(names.contains(&upload.as_str()), "{upload} is missing");
        assert_eq!(
            doc[index.as_str()]["rules"],
            doc[upload.as_str()]["rules"],
            "{upload} must run exactly when {index} does"
        );
        let var = format!("SOURCEGULL_{}_IMAGE", language.to_uppercase());
        let default = doc["variables"][var.as_str()].as_str();
        let autoindex = indexers::DEFAULT_INDEXERS
            .iter()
            .find(|(k, _)| *k == language)
            .map(|(_, v)| *v);
        assert!(autoindex.is_some(), "auto-indexing has no {language} image");
        assert_eq!(default, autoindex, "{var} must be auto-indexing's image");
    }
    for (name, job) in &jobs {
        assert!(
            job.get("rules").is_some(),
            "{name} has no rules (jobs are opt-in)"
        );
        let script = job["script"].as_sequence().expect("every job has a script");
        assert!(!script.is_empty());
        for need in job
            .get("needs")
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
        {
            let need = need.as_str().unwrap();
            assert!(names.contains(&need), "{name} needs unknown job {need}");
        }
        if name.ends_with("-upload") {
            assert!(
                script
                    .iter()
                    .any(|l| l.as_str().unwrap().starts_with("gull code-intel upload")),
                "{name} does not upload"
            );
        }
    }
}

/// No CLI image is assumed: without SOURCEGULL_CLI_IMAGE every index and upload job is skipped and
/// one job says what to set.
#[test]
fn gitlab_ci_template_requires_cli_image() {
    let doc: Value = serde_yaml::from_str(TEMPLATE).unwrap();
    assert_eq!(doc["variables"]["SOURCEGULL_CLI_IMAGE"].as_str(), Some(""));
    let empty = r#"$SOURCEGULL_CLI_IMAGE == null || $SOURCEGULL_CLI_IMAGE == """#;
    let rules = doc[".sourcegull-scip-rules"]["rules"]
        .as_sequence()
        .unwrap();
    assert!(
        rules
            .iter()
            .any(|r| r["if"].as_str() == Some(empty) && r["when"].as_str() == Some("never"))
    );
    let check = &doc[MISSING_CLI_IMAGE];
    let rules = check["rules"].as_sequence().unwrap();
    let last = rules.last().unwrap();
    assert_eq!(last["if"].as_str(), Some(empty));
    assert!(last.get("when").is_none());
    assert_eq!(check["allow_failure"].as_bool(), Some(true));
    let script = serde_yaml::to_string(&check["script"]).unwrap();
    assert!(script.contains("SOURCEGULL_CLI_IMAGE"));
    assert!(check.get("image").is_none());
}
