//! The public API: rendering, conditions, parse-only validation, error positions and
//! limits, plus a property test that arbitrary input never panics.

use proptest::prelude::*;
use sourcegull_batches_template::{
    ErrorKind, MissingKey, Options, Template, Value, contains_template_action, evaluate_bool,
    is_static, parse, render, render_with,
};

fn ctx() -> Value {
    Value::from_json(&serde_json::json!({
        "repository": { "name": "gitlab.com/acme/api", "branch": "main" },
        "batch_change": { "name": "bump", "description": "Bump things" },
        "steps": { "modified_files": ["a.go", "b.go"], "path": "" },
        "outputs": { "n": 2, "list": ["x", "y"], "obj": { "k": "v" } },
    }))
}

#[test]
fn renders_with_context_names_and_dot() {
    let c = ctx();
    assert_eq!(
        render("${{ repository.name }}@${{ .repository.branch }}", &c).unwrap(),
        "gitlab.com/acme/api@main"
    );
    assert_eq!(
        render(
            "${{ range $i, $f := steps.modified_files }}${{ if $i }},${{ end }}${{ $f }}${{ end }}",
            &c
        )
        .unwrap(),
        "a.go,b.go"
    );
    // JSON numbers are float64, as in Go.
    assert_eq!(render("${{ outputs.n }}", &c).unwrap(), "2");
    assert!(render("${{ eq outputs.n 2 }}", &c).is_err());
    assert_eq!(render("${{ eq outputs.n 2.0 }}", &c).unwrap(), "true");
}

#[test]
fn conditions() {
    let c = ctx();
    assert!(evaluate_bool("", &c).unwrap());
    assert!(evaluate_bool(" ${{ eq repository.branch \"main\" }} ", &c).unwrap());
    assert!(!evaluate_bool("${{ ne repository.branch \"main\" }}", &c).unwrap());
    assert!(!evaluate_bool("yes", &c).unwrap());
    assert!(evaluate_bool("${{ matches repository.name \"gitlab.com/*\" }}", &c).unwrap());
    assert!(evaluate_bool("${{ repository.nope }}", &c).is_err());
}

#[test]
fn errors_carry_positions() {
    let c = ctx();
    let e = render("line one\n  ${{ repository.nmae }}", &c).unwrap_err();
    assert_eq!((e.line, e.column), (2, 7), "{e}");
    assert_eq!(
        e.kind,
        ErrorKind::MissingKey {
            path: "repository.nmae".into(),
            key: "nmae".into()
        }
    );
    assert!(e.to_string().starts_with("2:7: "), "{e}");

    let e = render("ok ${{ upper repository.name }}", &c).unwrap_err();
    assert_eq!(e.kind, ErrorKind::UndefinedFunction("upper".into()));
    assert_eq!((e.line, e.column), (1, 8));
    assert!(e.message.contains("\"upper\" not defined"), "{e}");
    assert!(e.message.contains("Sprig"), "{e}");

    let e = render("${{ call repository }}", &c).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported("call".into()));

    let e = parse("a\nb ${{ if true }}").unwrap_err();
    assert_eq!(e.kind, ErrorKind::Syntax);

    let e = parse("${{ \"é\" | x }} ${{ $nope }}").unwrap_err();
    assert_eq!((e.line, e.column), (1, 20), "{e}");
}

#[test]
fn parse_only_validation() {
    let t = Template::parse("${{ join (split repository.name \"/\") \"-\" }} ${{ outputs.x }}")
        .unwrap();
    assert_eq!(
        t.function_names(),
        ["join", "split", "repository", "outputs"]
    );
    assert!(
        t.check_functions(|n| n == "repository" || n == "outputs")
            .is_ok()
    );
    assert!(t.check_functions(|n| n == "repository").is_err());
    assert!(!t.is_static());
    assert_eq!(
        t.tree_string(),
        "{{join (split repository.name \"/\") \"-\"}} {{outputs.x}}"
    );
    assert!(Template::parse("plain").unwrap().is_static());
    assert!(contains_template_action("a ${{ b }}"));
    assert!(is_static("{{ not ours }}"));
}

#[test]
fn missing_key_option() {
    let c = ctx();
    let opts = Options {
        missing_key: MissingKey::NoValue,
        ..Options::default()
    };
    assert_eq!(
        render_with("${{ repository.nope }}", &c, &c, &opts).unwrap(),
        "<no value>"
    );
}

#[test]
fn limits() {
    let c = ctx();
    let opts = Options {
        max_steps: 1000,
        ..Options::default()
    };
    let e = render_with("${{ range 100000000000 }}x${{ end }}", &c, &c, &opts).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Limit);
    let e = render_with(
        "${{ range $i := outputs.list }}${{ range 10000 }}x${{ end }}${{ end }}",
        &c,
        &c,
        &opts,
    )
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Limit);

    let opts = Options {
        max_output: 64,
        ..Options::default()
    };
    let e = render_with(
        "${{ $s := \"ab\" }}${{ range 10 }}${{ $s = replace $s \"\" $s }}${{ end }}${{ $s }}",
        &c,
        &c,
        &opts,
    )
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Limit);

    let deep = format!("${{{{ {}1{} }}}}", "(".repeat(500), ")".repeat(500));
    assert_eq!(parse(&deep).unwrap_err().kind, ErrorKind::Limit);
}

/// Fragments that make random input look like templates often enough to reach the
/// parser and executor.
fn fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("${{".to_owned()),
        Just("}}".to_owned()),
        Just("${{- ".to_owned()),
        Just(" -}}".to_owned()),
        Just("/*".to_owned()),
        Just("*/".to_owned()),
        Just(" ".to_owned()),
        Just("\n".to_owned()),
        Just("|".to_owned()),
        Just("(".to_owned()),
        Just(")".to_owned()),
        Just(":=".to_owned()),
        Just("=".to_owned()),
        Just(",".to_owned()),
        Just("$x".to_owned()),
        Just("$".to_owned()),
        Just(".".to_owned()),
        Just(".name".to_owned()),
        Just("repository".to_owned()),
        Just("outputs".to_owned()),
        Just("steps".to_owned()),
        Just("if".to_owned()),
        Just("else".to_owned()),
        Just("end".to_owned()),
        Just("range".to_owned()),
        Just("with".to_owned()),
        Just("break".to_owned()),
        Just("continue".to_owned()),
        Just("nil".to_owned()),
        Just("true".to_owned()),
        Just("eq".to_owned()),
        Just("index".to_owned()),
        Just("slice".to_owned()),
        Just("printf".to_owned()),
        Just("join".to_owned()),
        Just("split".to_owned()),
        Just("replace".to_owned()),
        Just("matches".to_owned()),
        Just("len".to_owned()),
        Just("and".to_owned()),
        Just("\"%d%s%v%-5.2f%[2]q%*d\"".to_owned()),
        Just("\"[a-{b,c}\"".to_owned()),
        Just("'x'".to_owned()),
        Just("`raw`".to_owned()),
        Just("\"\\u00e9\\x41\"".to_owned()),
        Just("1".to_owned()),
        Just("-2".to_owned()),
        Just("0x1F".to_owned()),
        Just("1e400".to_owned()),
        Just("3.5".to_owned()),
        Just("99999999999999999999".to_owned()),
        "[ -~é\u{1F600}]{0,3}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn never_panics_on_fragments(parts in prop::collection::vec(fragment(), 0..40)) {
        let s: String = parts.concat();
        let c = ctx();
        let opts = Options { max_steps: 100_000, max_output: 1 << 20, ..Options::default() };
        let _ = render_with(&s, &c, &c, &opts);
        if let Err(e) = parse(&s) {
            prop_assert!(e.offset <= s.len());
            prop_assert!(e.line >= 1 && e.column >= 1);
        }
    }

    #[test]
    fn never_panics_on_arbitrary_text(s in "\\PC{0,200}") {
        let c = ctx();
        let _ = render(&s, &c);
        let _ = render(&format!("${{{{ {s} }}}}"), &c);
        let _ = sourcegull_batches_template::validate_batch_spec_template(&s);
    }
}
