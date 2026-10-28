//! Wire format of the streaming search events.
//!
//! `fixtures/stream-schema.json` holds the reference events clients are written against, for each
//! event and match type: `full` with every field set and `minimal` with every optional field left
//! empty. Sourcegull's events, built with as many fields set as Sourcegull has and with as few, must use
//! only the reference field names with the same JSON types (at any depth), and send every field
//! the reference always sends, except the deliberate omissions in [`OMITTED`].

use std::collections::BTreeSet;

use serde_json::Value;
use sourcegull_search::stream::{
    Alert, ChunkMatch, Event, Filter, Location, Match, PersonUser, Progress, ProposedQuery, Range,
    Severity, Skipped, SkippedReason, SkippedSuggested, Symbol,
};

/// Fields the reference always sends that Sourcegull leaves out, with why.
const OMITTED: &[(&str, &str)] = &[
    // Always `null` in streamed events (decorated hunks belong to a GraphQL API). Clients read a
    // missing field as null.
    ("hunks", "always null when streamed"),
];

fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/stream-schema.json")).expect("fixture JSON")
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "integer",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Every field of `ours` exists in `reference` with the same JSON type. A `null` in the
/// reference stands for an empty list or object: anything matches it.
fn check_names_and_types(ours: &Value, reference: &Value, at: &str, errors: &mut Vec<String>) {
    match (ours, reference) {
        (_, Value::Null) => {}
        (Value::Object(o), Value::Object(f)) => {
            for (k, v) in o {
                match f.get(k) {
                    Some(fv) => check_names_and_types(v, fv, &format!("{at}.{k}"), errors),
                    None => errors.push(format!("{at}.{k}: not a field of the reference")),
                }
            }
        }
        (Value::Array(a), Value::Array(f)) => {
            if let Some(f0) = f.first() {
                for (i, v) in a.iter().enumerate() {
                    check_names_and_types(v, f0, &format!("{at}[{i}]"), errors);
                }
            }
        }
        (o, f) if kind(o) != kind(f) => errors.push(format!(
            "{at}: {} where the reference sends {}",
            kind(o),
            kind(f)
        )),
        _ => {}
    }
}

/// Every field of `minimal` (sent even when empty) is in `ours`, at any depth.
fn check_required(ours: &Value, minimal: &Value, at: &str, errors: &mut Vec<String>) {
    match (ours, minimal) {
        (Value::Object(o), Value::Object(m)) => {
            for (k, mv) in m {
                match o.get(k) {
                    Some(ov) => check_required(ov, mv, &format!("{at}.{k}"), errors),
                    None if OMITTED.iter().any(|(f, _)| f == k) => {}
                    None => errors.push(format!("{at}.{k}: always sent by the reference, missing")),
                }
            }
        }
        (Value::Array(a), Value::Array(m)) => {
            if let Some(m0) = m.first() {
                for (i, v) in a.iter().enumerate() {
                    check_required(v, m0, &format!("{at}[{i}]"), errors);
                }
            }
        }
        _ => {}
    }
}

/// Checks Sourcegull's full and minimal JSON against the reference.
fn compare(name: &str, reference: &Value, ours_full: &Value, ours_minimal: &Value) {
    let mut errors = Vec::new();
    for (label, ours) in [("full", ours_full), ("minimal", ours_minimal)] {
        let at = format!("{name} ({label})");
        check_names_and_types(ours, &reference["full"], &at, &mut errors);
        check_required(ours, &reference["minimal"], &at, &mut errors);
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

fn data(event: &Event) -> Value {
    serde_json::from_str(&event.data()).expect("event JSON")
}

fn loc(offset: u32, line: u32, column: u32) -> Location {
    Location {
        offset,
        line,
        column,
    }
}

fn range() -> Range {
    Range {
        start: loc(47, 5, 1),
        end: loc(58, 5, 12),
    }
}

const OID: &str = "4a3d4f6c6b0e0f6f0a8f9b6b3b1b4c3e2d1f0a9b";

fn content(full: bool) -> Match {
    Match::Content {
        path: "cmd/api/main.go".into(),
        repository_id: 7,
        repository: "gitlab.com/acme/api".into(),
        commit: OID.into(),
        branches: if full { vec!["main".into()] } else { vec![] },
        chunk_matches: if full {
            vec![ChunkMatch {
                content: "func main() {\n\tlog.Println(\"start\")".into(),
                content_start: loc(41, 4, 0),
                ranges: vec![range()],
            }]
        } else {
            vec![]
        },
        language: full.then(|| "Go".into()),
    }
}

fn path(full: bool) -> Match {
    Match::Path {
        path: "cmd/api/main.go".into(),
        path_matches: if full { vec![range()] } else { vec![] },
        repository_id: 7,
        repository: "gitlab.com/acme/api".into(),
        commit: OID.into(),
        branches: if full { vec!["main".into()] } else { vec![] },
        language: full.then(|| "Go".into()),
    }
}

fn symbol(full: bool) -> Match {
    Match::Symbol {
        path: "cmd/api/main.go".into(),
        repository_id: 7,
        repository: "gitlab.com/acme/api".into(),
        commit: OID.into(),
        branches: if full { vec!["main".into()] } else { vec![] },
        language: full.then(|| "Go".into()),
        symbols: if full {
            vec![Symbol {
                url: "/gitlab.com/acme/api/-/blob/cmd/api/main.go#L5:6-5:10".into(),
                name: "main".into(),
                container_name: String::new(),
                kind: "FUNCTION".into(),
                line: 5,
                range: range(),
            }]
        } else {
            vec![]
        },
    }
}

fn repo(full: bool) -> Match {
    Match::Repo {
        repository_id: 7,
        repository: "gitlab.com/acme/api".into(),
        description: full.then(|| "The API".into()),
        fork: full,
        archived: full,
        private: full,
    }
}

fn commit(content: &str, full: bool) -> Match {
    Match::Commit {
        label: "[gitlab.com/acme/api](/gitlab.com/acme/api) › [Ann](/c): [Fix start](/c)".into(),
        url: format!("/gitlab.com/acme/api/-/commit/{OID}"),
        detail: "[`4a3d4f6` 2023-06-01](/c)".into(),
        repository_id: 7,
        repository: "gitlab.com/acme/api".into(),
        oid: OID.into(),
        message: "Fix start\n".into(),
        author_name: "Ann".into(),
        author_date: "2023-06-01T10:00:00Z".into(),
        committer_name: "Ann".into(),
        committer_date: "2023-06-01T10:00:00Z".into(),
        content: content.into(),
        ranges: if full { vec![[1, 0, 3]] } else { vec![] },
    }
}

#[test]
fn match_events_have_the_reference_shapes() {
    let fx = fixtures();
    let reference = &fx["matches"];
    let one = |m: Match| data(&Event::Matches(vec![m]))[0].clone();
    let cases: Vec<(&str, Match, Match)> = vec![
        ("content", content(true), content(false)),
        ("path", path(true), path(false)),
        ("symbol", symbol(true), symbol(false)),
        ("repo", repo(true), repo(false)),
        (
            "commit",
            commit("```COMMIT_EDITMSG\nFix start\n```", true),
            commit("```COMMIT_EDITMSG\n\n```", false),
        ),
        (
            "diff",
            commit(
                "```diff\nmain.go main.go\n@@ -1,1 +1,1 @@ \n-old\n+start\n```",
                true,
            ),
            commit("```diff\n\n```", false),
        ),
        (
            "person",
            Match::Person {
                handle: "ann".into(),
                email: "ann@example.com".into(),
                user: Some(PersonUser {
                    username: "ann".into(),
                    display_name: "Ann".into(),
                    avatar_url: "https://example.com/ann.png".into(),
                }),
            },
            Match::Person {
                handle: String::new(),
                email: String::new(),
                user: None,
            },
        ),
        (
            "team",
            Match::Team {
                handle: "core".into(),
                email: "core@example.com".into(),
                name: "core".into(),
                display_name: "Core".into(),
            },
            Match::Team {
                handle: String::new(),
                email: String::new(),
                name: String::new(),
                display_name: String::new(),
            },
        ),
    ];
    let mut types = BTreeSet::new();
    for (name, full, minimal) in cases {
        let (f, m) = (one(full), one(minimal));
        assert_eq!(
            f["type"], reference[name]["full"]["type"],
            "{name}: type tag"
        );
        types.insert(name);
        compare(name, &reference[name], &f, &m);
    }
    // Every match type of the fixture is covered.
    let fixture_types: BTreeSet<&str> = reference
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "content-line-matches")
        .collect();
    assert_eq!(types, fixture_types);
}

#[test]
fn content_matches_in_line_match_form() {
    // Without `cm=t` the stream sends `lineMatches` instead of `chunkMatches`, as clients expect.
    let fx = fixtures();
    let line_form = |m: Match| -> Value {
        let ev = Event::Matches(vec![m]);
        serde_json::from_str::<Value>(&ev.data_with_line_matches()).unwrap()[0].clone()
    };
    let (f, m) = (line_form(content(true)), line_form(content(false)));
    assert!(f.get("chunkMatches").is_none());
    assert_eq!(
        f["lineMatches"],
        serde_json::json!([{"line": "\tlog.Println(\"start\")", "lineNumber": 5, "offsetAndLengths": [[1, 11]]}])
    );
    compare(
        "content-line-matches",
        &fx["matches"]["content-line-matches"],
        &f,
        &m,
    );
}

fn skipped(reason: SkippedReason, suggested: bool) -> Skipped {
    Skipped {
        reason,
        title: "1 archived".into(),
        message: "By default we exclude archived repositories.".into(),
        severity: Severity::Info,
        suggested: suggested.then(|| SkippedSuggested {
            title: "include archived".into(),
            query_expression: "archived:yes".into(),
        }),
    }
}

#[test]
fn progress_events_have_the_reference_shape() {
    let fx = fixtures();
    let full = Progress {
        done: true,
        repositories_count: Some(3),
        match_count: 42,
        duration_ms: 120,
        skipped: vec![skipped(SkippedReason::ExcludedArchive, true)],
    };
    let minimal = Progress::default();
    compare(
        "progress",
        &fx["progress"],
        &data(&Event::Progress(full)),
        &data(&Event::Progress(minimal)),
    );
    // Reasons and severities use the reference names.
    let names: BTreeSet<String> = fx["skipped-reasons"]
        .as_array()
        .unwrap()
        .iter()
        .chain(fx["client-skipped-reasons"].as_array().unwrap())
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let reasons = [
        SkippedReason::DocumentMatchLimit,
        SkippedReason::ShardMatchLimit,
        SkippedReason::RepositoryLimit,
        SkippedReason::ShardTimeout,
        SkippedReason::RepositoryCloning,
        SkippedReason::RepositoryMissing,
        SkippedReason::BackendMissing,
        SkippedReason::ExcludedFork,
        SkippedReason::ExcludedArchive,
        SkippedReason::DisplayLimit,
        SkippedReason::Error,
    ];
    for reason in reasons {
        let v = serde_json::to_value(skipped(reason, false)).unwrap();
        let name = v["reason"].as_str().unwrap();
        assert!(names.contains(name), "{reason:?} is sent as {name:?}");
    }
    assert_eq!(
        serde_json::to_value(skipped(SkippedReason::DisplayLimit, false)).unwrap()["reason"],
        "display"
    );
    assert_eq!(
        serde_json::to_value(skipped(SkippedReason::ExcludedFork, false)).unwrap()["reason"],
        "repository-fork"
    );
    for (severity, name) in [(Severity::Info, "info"), (Severity::Warn, "warn")] {
        assert_eq!(serde_json::to_value(severity).unwrap(), name);
        assert!(
            fx["severities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == name)
        );
    }
}

#[test]
fn filters_alerts_errors_and_done_have_the_reference_shapes() {
    let fx = fixtures();
    let filter = |count| Filter {
        value: "lang:go".into(),
        label: "Go".into(),
        count,
        exhaustive: true,
        kind: "lang".into(),
    };
    compare(
        "filters",
        &fx["filters"],
        &data(&Event::Filters(vec![filter(12)])),
        &data(&Event::Filters(vec![filter(0)])),
    );
    let full = Alert {
        title: "No repositories found".into(),
        description: "Check your repo: filter.".into(),
        proposed_queries: vec![ProposedQuery {
            description: "include forks".into(),
            query: "repo:acme fork:yes".into(),
        }],
    };
    let minimal = Alert {
        title: String::new(),
        description: String::new(),
        proposed_queries: vec![ProposedQuery {
            description: String::new(),
            query: "repo:acme".into(),
        }],
    };
    compare(
        "alert",
        &fx["alert"],
        &data(&Event::Alert(full)),
        &data(&Event::Alert(minimal)),
    );
    compare(
        "error",
        &fx["error"],
        &data(&Event::Error("invalid query".into())),
        &data(&Event::Error(String::new())),
    );
    compare(
        "done",
        &fx["done"],
        &data(&Event::Done),
        &data(&Event::Done),
    );
    for (event, name) in [
        (Event::Matches(vec![]), "matches"),
        (Event::Progress(Progress::default()), "progress"),
        (Event::Filters(vec![]), "filters"),
        (Event::Alert(minimal_alert()), "alert"),
        (Event::Error(String::new()), "error"),
        (Event::Done, "done"),
    ] {
        assert_eq!(event.name(), name);
    }
}

fn minimal_alert() -> Alert {
    Alert {
        title: String::new(),
        description: String::new(),
        proposed_queries: vec![],
    }
}
