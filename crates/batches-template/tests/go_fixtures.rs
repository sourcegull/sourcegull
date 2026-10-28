//! Golden tests against Go: `fixtures/go_text_template.json` holds, for each case in
//! `fixtures/cases.json`, what Go's `text/template` (Go 1.23) printed or that it failed, set up
//! like bulk change spec templates (delimiters `${{ }}`, the bulk change spec functions, the data's top-level
//! keys as niladic functions and the data as the dot). Regenerate with the command in
//! `fixtures/gen/main.go`.

use sourcegull_batches_template::{MissingKey, Options, Value, render_with};

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    template: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    missingkey: Option<String>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Cases where this evaluator deliberately differs from Go 1.23, and why.
const KNOWN_DIFFERENCES: &[(&str, &str)] = &[
    (
        "range int",
        "ranging over an integer is supported, as in Go 1.24's text/template",
    ),
    (
        "range int var",
        "ranging over an integer is supported, as in Go 1.24's text/template",
    ),
    (
        "join empty",
        "an empty JSON list is []interface{} in the Go fixture; lists of strings are accepted as []string",
    ),
    (
        "define keyword",
        "define/template/block are rejected as unsupported",
    ),
    ("complex", "complex numbers are rejected as unsupported"),
];

#[test]
fn matches_go_text_template() {
    let raw = include_str!("fixtures/go_text_template.json");
    let cases: Vec<Case> = serde_json::from_str(raw).expect("fixtures parse");
    assert!(cases.len() > 150, "fixtures are missing cases");
    let mut failures = Vec::new();
    for c in &cases {
        let data = c
            .data
            .as_ref()
            .map(Value::from_json_yaml_numbers)
            .unwrap_or_else(|| Value::Map(Default::default()));
        let opts = Options {
            missing_key: match c.missingkey.as_deref() {
                Some("default" | "zero" | "invalid") => MissingKey::NoValue,
                _ => MissingKey::Error,
            },
            ..Options::default()
        };
        let got = render_with(&c.template, &data, &data, &opts);
        if KNOWN_DIFFERENCES.iter().any(|(n, _)| *n == c.name) {
            continue;
        }
        let ok = match (&got, &c.output, &c.error) {
            (Ok(out), Some(want), _) => out == want,
            (Err(_), None, Some(_)) => true,
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "{}: template {:?}\n  go:   {}\n  ours: {}",
                c.name,
                c.template,
                match (&c.output, &c.error) {
                    (Some(o), _) => format!("{o:?}"),
                    (_, Some(e)) => format!("error {e}"),
                    _ => "?".to_owned(),
                },
                match &got {
                    Ok(o) => format!("{o:?}"),
                    Err(e) => format!("error {e}"),
                }
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from Go:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn known_differences_are_extensions_or_clear_errors() {
    let empty = Value::Map(Default::default());
    let opts = Options::default();
    assert_eq!(
        render_with("${{ range 3 }}${{ . }}${{ end }}", &empty, &empty, &opts).unwrap(),
        "012"
    );
    let err = render_with("${{ define \"x\" }}y${{ end }}", &empty, &empty, &opts).unwrap_err();
    assert!(err.message.contains("not supported"), "{err}");
    let err = render_with("${{ 1i }}", &empty, &empty, &opts).unwrap_err();
    assert!(err.message.contains("not supported"), "{err}");
}
