//! Bulk change spec parsing and validation against the fixtures in `tests/fixtures`:
//!
//! - `valid/*`: must parse; warnings must equal `<name>.expected` (none when it is missing).
//! - `invalid/*`: must fail with exactly the issues in `<name>.expected`.
//! - `examples/*`: realistic specs covering the format's features; must parse.
//!
//! `.expected` lines are `<line>:<column> <message>` (`-:-` without a position), warnings prefixed
//! with `warning `. `SOURCEGULL_BLESS=1` rewrites them from the current output.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use sourcegull_batches::spec::{
    IfCondition, OnQueryOrRepository, OutputFormat, Published, PublishedValue,
};
use sourcegull_batches::{BatchSpec, SpecIssue, parse_batch_spec};

fn fixtures(sub: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e == "yaml" || e == "yml" || e == "json")
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures in {}", dir.display());
    files
}

fn render(issue: &SpecIssue, prefix: &str) -> String {
    let at = issue
        .location
        .map_or("-:-".to_string(), |l| format!("{}:{}", l.line, l.column));
    format!("{prefix}{at} {}", issue.message)
}

fn expected_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".expected");
    PathBuf::from(s)
}

/// Compares `got` with the sidecar file (or writes it under `SOURCEGULL_BLESS=1`).
fn check_expected(p: &Path, got: &[String]) {
    let path = expected_path(p);
    if std::env::var_os("SOURCEGULL_BLESS").is_some() {
        if got.is_empty() {
            let _ = fs::remove_file(&path);
        } else {
            fs::write(&path, got.join("\n") + "\n").unwrap();
        }
        return;
    }
    let want: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(got, want, "{}", p.display());
}

#[test]
fn valid_fixtures() {
    for p in fixtures("valid") {
        let text = fs::read_to_string(&p).unwrap();
        let parsed = parse_batch_spec(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let got: Vec<String> = parsed
            .warnings
            .iter()
            .map(|w| render(w, "warning "))
            .collect();
        check_expected(&p, &got);
    }
}

#[test]
fn invalid_fixtures() {
    for p in fixtures("invalid") {
        let text = fs::read_to_string(&p).unwrap();
        let errors = match parse_batch_spec(&text) {
            Ok(_) => panic!("{} parsed", p.display()),
            Err(e) => e,
        };
        for issue in errors.issues() {
            assert!(
                issue.location.is_some(),
                "{}: {issue} has no position",
                p.display()
            );
        }
        let got: Vec<String> = errors.issues().iter().map(|i| render(i, "")).collect();
        check_expected(&p, &got);
    }
}

#[test]
fn example_specs() {
    // `quoted-continuation` has a top-level multi-line double-quoted scalar whose continuation
    // lines are not indented, which libyaml accepts.
    let mut parsed = 0;
    for p in fixtures("examples") {
        let name = p.file_name().unwrap().to_str().unwrap();
        let text = fs::read_to_string(&p).unwrap();
        match parse_batch_spec(&text) {
            Ok(_) => parsed += 1,
            Err(e) => panic!("{name}: {e}"),
        }
    }
    assert!(parsed >= 13, "{parsed}");
}

#[test]
fn unindented_quoted_continuation_lines() {
    // The continuation lines' (missing) indentation does not matter.
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/examples/quoted-continuation.batch.yaml"),
    )
    .unwrap();
    let spec = BatchSpec::parse(&text).unwrap();
    assert_eq!(
        spec.description,
        "Rewrites the import path of the logging package from\n\
         `example.com/acme/log/v1`\nto\n`example.com/acme/log/v2`"
    );
    assert_eq!(spec.steps.len(), 2);
}

fn full() -> BatchSpec {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid/full.batch.yaml"),
    )
    .unwrap();
    BatchSpec::parse(&text).unwrap()
}

#[test]
fn full_spec_fields() {
    let spec = full();
    assert_eq!(spec.version, 2);
    assert_eq!(spec.name, "update-go-deps.v2");
    assert_eq!(spec.description, "Bumps dependencies in every Go module.\n");

    assert_eq!(spec.on.len(), 3);
    assert_eq!(spec.on[0].to_string(), "file:go.mod lang:go");
    let OnQueryOrRepository::Repository(api) = &spec.on[1] else {
        panic!()
    };
    assert_eq!(api.branches(), ["release-1.2"]);
    let OnQueryOrRepository::Repository(web) = &spec.on[2] else {
        panic!()
    };
    assert_eq!(web.branches(), ["main", "next"]);
    assert_eq!(
        spec.on[2].to_string(),
        "repository:gitlab.example.com/platform/web"
    );

    let ws = &spec.workspaces[0];
    assert_eq!(ws.root_at_location_of, "go.mod");
    assert!(ws.only_fetch_workspace);
    let glob = ws.in_glob().unwrap().unwrap();
    assert!(glob.is_match("gitlab.example.com/platform/api"));
    assert!(!glob.is_match("gitlab.example.com/other/api"));

    let s0 = &spec.steps[0];
    assert_eq!(s0.container, "golang:1.22");
    assert!(s0.env.is_static());
    assert_eq!(
        s0.outputs["modFiles"].value,
        "${{ join steps.modified_files \" \" }}"
    );
    assert_eq!(s0.outputs["modFiles"].format(), OutputFormat::Text);
    assert_eq!(s0.outputs["report"].format(), OutputFormat::Json);
    assert_eq!(s0.if_condition(), None);

    let s1 = &spec.steps[1];
    assert_eq!(s1.env.outer_vars(), ["GITLAB_TOKEN"]);
    let outer = HashMap::from([("GITLAB_TOKEN".to_string(), "glpat-x".to_string())]);
    let env = s1.env.resolve(&outer);
    assert_eq!(env["GITLAB_TOKEN"], "glpat-x");
    assert_eq!(env["LEVEL"], "debug");
    assert_eq!(s1.files["/tmp/script.sh"], "echo ${{ outputs.modFiles }}\n");
    assert_eq!(s1.mount[0].mountpoint, "/scripts");
    assert_eq!(
        s1.if_condition().as_deref(),
        Some("${{ matches repository.name \"gitlab.example.com/platform/*\" }}")
    );
    assert_eq!(spec.steps[2].if_, Some(IfCondition::Bool(false)));
    assert_eq!(spec.steps[2].if_condition().as_deref(), Some("false"));

    let tc = spec.transform_changes.as_ref().unwrap();
    assert_eq!(
        tc.groups_for_repository("gitlab.example.com/platform/web")
            .len(),
        1
    );
    let api_groups = tc.groups_for_repository("gitlab.example.com/platform/api");
    assert_eq!(api_groups.len(), 2);
    let tmpl = spec.changeset_template.as_ref().unwrap();
    assert!(
        sourcegull_batches::spec::TransformChanges::validate_groups(
            "gitlab.example.com/platform/api",
            &tmpl.branch,
            &api_groups
        )
        .is_ok()
    );

    let ids: Vec<String> = spec.import_changesets[0]
        .external_ids
        .iter()
        .map(|id| id.to_string())
        .collect();
    assert_eq!(ids, ["12", "13"]);

    assert_eq!(tmpl.fork, Some(false));
    let author = tmpl.commit.author.as_ref().unwrap();
    assert_eq!(author.email, "batch-bot@example.com");
}

#[test]
fn published_resolution() {
    let spec = full();
    let tmpl = spec.changeset_template.unwrap();
    let p = tmpl.published.as_ref().unwrap();
    use PublishedValue as V;
    assert_eq!(
        p.value_for("gitlab.example.com/other/x"),
        Some(V::Unpublished)
    );
    assert_eq!(
        p.value_for("gitlab.example.com/platform/web"),
        Some(V::Published)
    );
    assert_eq!(
        p.value_for("gitlab.example.com/platform/ops"),
        Some(V::Draft)
    );
    assert_eq!(
        p.value_for("gitlab.example.com/platform/api"),
        Some(V::Published)
    );
    // The `@branch` rule only applies to that changeset branch.
    assert_eq!(
        tmpl.published_for("gitlab.example.com/platform/web", "update-go-deps"),
        Some(V::Draft)
    );
    assert_eq!(
        tmpl.published_for("gitlab.example.com/platform/web", "update-go-deps-tools"),
        Some(V::Published)
    );

    let draft = BatchSpec::parse(
        "name: d\nsteps: [{run: x, container: y}]\nchangesetTemplate: {title: t, branch: b, commit: {message: m}, published: draft}\n",
    )
    .unwrap();
    let t = draft.changeset_template.unwrap();
    assert_eq!(t.published, Some(Published::All(V::Draft)));
    assert_eq!(t.published_for("anything", "b"), Some(V::Draft));

    let unset = BatchSpec::parse(
        "name: d\nsteps: [{run: x, container: y}]\nchangesetTemplate: {title: t, branch: b, commit: {message: m}}\n",
    )
    .unwrap();
    assert_eq!(
        unset.changeset_template.unwrap().published_for("a", "b"),
        None
    );
}

#[test]
fn json_round_trip() {
    for p in fixtures("valid").into_iter().chain(fixtures("examples")) {
        let text = fs::read_to_string(&p).unwrap();
        let Ok(spec) = BatchSpec::parse(&text) else {
            continue;
        };
        let json = serde_json::to_string_pretty(&spec).unwrap();
        let again =
            BatchSpec::parse(&json).unwrap_or_else(|e| panic!("{}: {e}\n{json}", p.display()));
        assert_eq!(spec, again, "{}", p.display());
    }
}

#[test]
fn if_condition_forms() {
    for (raw, want) in [
        (r#""true""#, "true"),
        (r#""false""#, "false"),
        ("true", "true"),
        ("false", "false"),
        (r#""${{ foobar }}""#, "${{ foobar }}"),
        ("${{ foobar }}", "${{ foobar }}"),
        ("foobar", "foobar"),
    ] {
        let text = format!(
            "name: hello-world\nsteps:\n  - run: echo\n    if: {raw}\n    container: alpine:3\nchangesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n"
        );
        let spec = BatchSpec::parse(&text).unwrap();
        assert_eq!(spec.steps[0].if_condition().as_deref(), Some(want), "{raw}");
    }
}

#[test]
fn error_display() {
    let e = BatchSpec::parse("name: a b\nversion: 9\n").unwrap_err();
    assert_eq!(
        e.to_string(),
        "2 errors occurred:\n\t* line 1, column 7: The bulk change name can only contain word characters, dots and dashes. No whitespace or newlines allowed.\n\t* line 2, column 10: version: must be one of the following: 1, 2"
    );
}

const VALID: &str = include_str!("fixtures/valid/full.batch.yaml");

fn mutate(text: &str, edits: &[(usize, u8, u8)]) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for &(pos, op, b) in edits {
        if bytes.is_empty() {
            break;
        }
        let i = pos % bytes.len();
        const CHARS: &[u8] = b" \n\t:-[]{},&*!|>'\"#%@`?~0aZ$\\";
        let c = CHARS[b as usize % CHARS.len()];
        match op % 3 {
            0 => {
                bytes.remove(i);
            }
            1 => bytes.insert(i, c),
            _ => bytes[i] = c,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn yamlish() -> impl Strategy<Value = String> {
    let token = prop::sample::select(vec![
        "name: x",
        "on:",
        "steps:",
        "- ",
        "  ",
        "    ",
        "\n",
        "run: echo",
        "container: a",
        "changesetTemplate:",
        "published:",
        "- \"*\": true",
        "[",
        "]",
        "{",
        "}",
        ",",
        ": ",
        "&a ",
        "*a",
        "<<: *a",
        "!!str ",
        "!!int ",
        "~",
        "null",
        "1e999",
        ".inf",
        "\"",
        "'",
        "|",
        ">-",
        "---",
        "...",
        "? ",
        "#",
        "env:",
        "- A",
        "outputs:",
        "if: ${{ x }}",
        "\t",
        "é",
        "0x",
    ]);
    prop::collection::vec(token, 0..60).prop_map(|t| t.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_text(text in any::<String>()) {
        let _ = parse_batch_spec(&text);
        let _ = sourcegull_batches::parse_changeset_spec(&text);
    }

    #[test]
    fn never_panics_on_yaml_like_text(text in yamlish()) {
        let _ = parse_batch_spec(&text);
    }

    #[test]
    fn never_panics_on_mutated_specs(
        edits in prop::collection::vec((any::<usize>(), any::<u8>(), any::<u8>()), 1..8)
    ) {
        let text = mutate(VALID, &edits);
        if let Err(e) = parse_batch_spec(&text) {
            // Every error of a mutated spec still has a position.
            for issue in e.issues() {
                prop_assert!(issue.location.is_some(), "{}", issue);
            }
        }
    }
}
