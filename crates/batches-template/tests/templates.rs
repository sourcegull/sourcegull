//! Step and changeset template rendering, `if:` conditions, partial evaluation and bulk change spec
//! template validation.

use std::collections::BTreeMap;

use sourcegull_batches_template::{
    BatchChangeAttributes, Changes, ChangesetTemplateContext, Repository, StepContext, StepResult,
    StepsContext, Value, validate_batch_spec_template,
};

fn test_repo1() -> Repository {
    Repository {
        name: "gitlab.example.com/acme/tools".into(),
        branch: String::new(),
        file_matches: vec!["main.go".into(), "README.md".into()],
    }
}

fn test_changes() -> Changes {
    Changes {
        modified: vec!["go.mod".into()],
        added: vec!["main.go.swp".into()],
        deleted: vec![".DS_Store".into()],
        renamed: vec!["new-filename.txt".into()],
    }
}

/// A `format: yaml` output value.
fn parsed_yaml() -> Value {
    Value::map([
        ("dist", Value::from("release")),
        ("env", Value::strings(["GO111MODULE=on", "CGO_ENABLED=0"])),
        (
            "before",
            Value::map([(
                "hooks",
                Value::strings(["go mod download", "go mod tidy", "go generate ./schema"]),
            )]),
        ),
    ])
}

#[test]
fn validate_batch_spec_template_cases() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            "full bulk change spec, all valid template variables",
            "name: valid-batch-spec
				on:
				- repository: gitlab.example.com/acme/fake

				steps:
				- run: |
						${{ repository.search_result_paths }}
						${{ repository.name }}
						${{ batch_change.name }}
						${{ batch_change.description }}
						${{ previous_step.modified_files }}
						${{ previous_step.added_files }}
						${{ previous_step.deleted_files }}
						${{ previous_step.renamed_files }}
						${{ previous_step.stdout }}
						${{ previous_step.stderr}}
						${{ step.modified_files }}
						${{ step.added_files }}
						${{ step.deleted_files }}
						${{ step.renamed_files }}
						${{ step.stdout}}
						${{ step.stderr}}
						${{ steps.modified_files }}
						${{ steps.added_files }}
						${{ steps.deleted_files }}
						${{ steps.renamed_files }}
						${{ steps.path }}
					container: my-container

				changesetTemplate:
				title: |
					${{ repository.search_result_paths }}
					${{ repository.name }}
					${{ repository.branch }}
					${{ batch_change.name }}
					${{ batch_change.description }}
					${{ steps.modified_files }}
					${{ steps.added_files }}
					${{ steps.deleted_files }}
					${{ steps.renamed_files }}
					${{ steps.path }}
					${{ batch_change_link }}
					body: I'm a changeset yay!
					branch: my-branch
					commit:
						message: I'm a changeset yay!
					",
            None,
        ),
        (
            "valid template helpers",
            r#"${{ join repository.search_result_paths "\n" }}
				${{ join_if "---" "a" "b" "" "d" }}
				${{ replace "a/b/c/d" "/" "-" }}
				${{ split repository.name "/" }}
				${{ matches repository.name "gitlab.example.com/my-org/terra*" }}
				${{ index steps.modified_files 1 }}"#,
            None,
        ),
        (
            "invalid step template variable",
            "${{ resipotory.search_result_paths }}",
            Some("validating bulk change spec template: unknown templating variable: 'resipotory'"),
        ),
        (
            "invalid step template variable, 1 level nested",
            "${{ repository.search_resalt_paths }}",
            Some(
                "validating bulk change spec template: unknown templating variable: 'repository.search_resalt_paths'",
            ),
        ),
        (
            "invalid changeset template variable",
            "${{ batch_chang_link }}",
            Some(
                "validating bulk change spec template: unknown templating variable: 'batch_chang_link'",
            ),
        ),
        (
            "invalid changeset template variable, 1 level nested",
            "${{ steps.mofidied_files }}",
            Some(
                "validating bulk change spec template: unknown templating variable: 'steps.mofidied_files'",
            ),
        ),
        (
            "escaped templating (CI expression syntax) is ignored",
            r#"${{ "${{ ignore_me }}" }}"#,
            None,
        ),
        (
            "output variables are ignored",
            r#"${{ outputs.IDontExist }}
						${{OUTPUTS.anotherOne}}
						${{ join outputs.myArray "," }}
						${{ index outputs.env.something 1 }}"#,
            None,
        ),
        (
            "output variables are ignored, but invalid step template variable still fails",
            "${{ outputs.unknown }} ${{ outputz.unknown }}",
            Some("validating bulk change spec template: unknown templating variable: 'outputz'"),
        ),
    ];
    for (name, spec, want) in cases {
        let got = validate_batch_spec_template(spec);
        match (want, got) {
            (None, Ok(())) => {}
            (Some(want), Err(e)) => assert_eq!(e.message, *want, "{name}"),
            (want, got) => panic!("{name}: want {want:?}, got {got:?}"),
        }
    }
}

#[test]
fn validation_errors_point_into_the_spec() {
    let e = validate_batch_spec_template(
        "name: x\nsteps:\n  - run: echo ${{ outputs.a }} ${{ repo.name }}\n",
    )
    .unwrap_err();
    assert_eq!((e.line, e.column), (3, 36), "{e}");
}

#[test]
fn eval_step_condition() {
    let ctx = StepContext {
        batch_change: BatchChangeAttributes {
            name: "test-batch-change".into(),
            description: "This bulk change is just an experiment".into(),
        },
        previous_step: StepResult {
            changed_files: test_changes(),
            stdout: "this is previous step's stdout".into(),
            stderr: "this is previous step's stderr".into(),
        },
        steps: StepsContext {
            changes: test_changes(),
            path: "sub/directory/of/repo".into(),
        },
        outputs: BTreeMap::new(),
        // The step result is not set when a condition is evaluated.
        step: StepResult::default(),
        repository: test_repo1(),
    };
    for (run, want) in [
        ("true", true),
        ("  true    ", true),
        ("TRUE", false),
        ("false", false),
        ("FALSE", false),
        (
            r#"${{ eq repository.name "gitlab.example.com/acme/tools" }}"#,
            true,
        ),
        (r#"${{ eq steps.path "sub/directory/of/repo" }}"#, true),
        (
            r#"${{ matches repository.name "gitlab.example.com/acme/*" }}"#,
            true,
        ),
    ] {
        assert_eq!(ctx.eval_condition(run).unwrap(), want, "{run}");
    }
    assert!(ctx.eval_condition("").unwrap());
}

const STEP_TEMPLATE: &str = "${{ repository.search_result_paths }}
${{ repository.name }}
${{ batch_change.name }}
${{ batch_change.description }}
${{ previous_step.modified_files }}
${{ previous_step.added_files }}
${{ previous_step.deleted_files }}
${{ previous_step.renamed_files }}
${{ previous_step.stdout }}
${{ previous_step.stderr}}
${{ outputs.lastLine }}
${{ index outputs.project.env 1 }}
${{ step.modified_files }}
${{ step.added_files }}
${{ step.deleted_files }}
${{ step.renamed_files }}
${{ step.stdout}}
${{ step.stderr}}
${{ steps.modified_files }}
${{ steps.added_files }}
${{ steps.deleted_files }}
${{ steps.renamed_files }}
${{ steps.path }}
";

#[test]
fn render_step_template() {
    let ctx = StepContext {
        batch_change: BatchChangeAttributes {
            name: "test-batch-change".into(),
            description: "This bulk change is just an experiment".into(),
        },
        previous_step: StepResult {
            changed_files: test_changes(),
            stdout: "this is previous step's stdout".into(),
            stderr: "this is previous step's stderr".into(),
        },
        outputs: BTreeMap::from([
            ("lastLine".to_owned(), Value::from("lastLine is this")),
            ("project".to_owned(), parsed_yaml()),
        ]),
        step: StepResult {
            changed_files: test_changes(),
            stdout: "this is current step's stdout".into(),
            stderr: "this is current step's stderr".into(),
        },
        steps: StepsContext {
            changes: test_changes(),
            path: "sub/directory/of/repo".into(),
        },
        repository: test_repo1(),
    };
    let want = "README.md main.go
gitlab.example.com/acme/tools
test-batch-change
This bulk change is just an experiment
[go.mod]
[main.go.swp]
[.DS_Store]
[new-filename.txt]
this is previous step's stdout
this is previous step's stderr
lastLine is this
CGO_ENABLED=0
[go.mod]
[main.go.swp]
[.DS_Store]
[new-filename.txt]
this is current step's stdout
this is current step's stderr
[go.mod]
[main.go.swp]
[.DS_Store]
[new-filename.txt]
sub/directory/of/repo
";
    assert_eq!(
        ctx.render(STEP_TEMPLATE).unwrap(),
        want,
        "lower-case aliases"
    );

    let empty = "${{ repository.search_result_paths }}
${{ repository.name }}
${{ previous_step.modified_files }}
${{ previous_step.added_files }}
${{ previous_step.deleted_files }}
${{ previous_step.renamed_files }}
${{ previous_step.stdout }}
${{ previous_step.stderr}}
${{ step.modified_files }}
${{ step.added_files }}
${{ step.deleted_files }}
${{ step.renamed_files }}
${{ step.stdout}}
${{ step.stderr}}
${{ steps.modified_files }}
${{ steps.added_files }}
${{ steps.deleted_files }}
${{ steps.renamed_files }}
${{ steps.path }}
";
    let want_empty = "

[]
[]
[]
[]


[]
[]
[]
[]


[]
[]
[]
[]

";
    assert_eq!(
        StepContext::default().render(empty).unwrap(),
        want_empty,
        "empty context"
    );
}

#[test]
fn render_step_map() {
    let ctx = StepContext {
        previous_step: StepResult {
            changed_files: test_changes(),
            stdout: "this is previous step's stdout".into(),
            stderr: "this is previous step's stderr".into(),
        },
        repository: test_repo1(),
        ..StepContext::default()
    };
    let input = BTreeMap::from([
        (
            "/tmp/my-file.txt".to_owned(),
            "${{ previous_step.modified_files }}".to_owned(),
        ),
        (
            "/tmp/my-other-file.txt".to_owned(),
            "${{ previous_step.added_files }}".to_owned(),
        ),
        (
            "/tmp/my-other-file2.txt".to_owned(),
            "${{ previous_step.deleted_files }}".to_owned(),
        ),
    ]);
    let want = BTreeMap::from([
        ("/tmp/my-file.txt".to_owned(), "[go.mod]".to_owned()),
        (
            "/tmp/my-other-file.txt".to_owned(),
            "[main.go.swp]".to_owned(),
        ),
        (
            "/tmp/my-other-file2.txt".to_owned(),
            "[.DS_Store]".to_owned(),
        ),
    ]);
    assert_eq!(ctx.render_map(&input).unwrap(), want);
}

#[test]
fn render_changeset_template_field() {
    let ctx = ChangesetTemplateContext {
        batch_change_attributes: BatchChangeAttributes {
            name: "test-batch-change".into(),
            description: "This bulk change is just an experiment".into(),
        },
        outputs: BTreeMap::from([
            ("lastLine".to_owned(), Value::from("lastLine is this")),
            ("project".to_owned(), parsed_yaml()),
        ]),
        repository: test_repo1(),
        steps: StepsContext {
            changes: Changes {
                modified: vec!["modified-file.txt".into()],
                added: vec!["added-file.txt".into()],
                deleted: vec!["deleted-file.txt".into()],
                renamed: vec!["renamed-file.txt".into()],
            },
            path: "infrastructure/sub-project".into(),
        },
    };
    let tmpl = "${{ repository.search_result_paths }}
${{ repository.name }}
${{ batch_change.name }}
${{ batch_change.description }}
${{ outputs.lastLine }}
${{ index outputs.project.env 1 }}
${{ steps.modified_files }}
${{ steps.added_files }}
${{ steps.deleted_files }}
${{ steps.renamed_files }}
${{ steps.path }}
${{ batch_change_link }}
";
    let want = "README.md main.go
gitlab.example.com/acme/tools
test-batch-change
This bulk change is just an experiment
lastLine is this
CGO_ENABLED=0
[modified-file.txt]
[added-file.txt]
[deleted-file.txt]
[renamed-file.txt]
infrastructure/sub-project
${{ batch_change_link }}";
    assert_eq!(ctx.render_field(tmpl).unwrap(), want, "lower-case aliases");

    let empty = "${{ repository.search_result_paths }}
${{ repository.name }}
${{ steps.modified_files }}
${{ steps.added_files }}
${{ steps.deleted_files }}
${{ steps.renamed_files }}
${{ batch_change_link }}
";
    let want_empty = "[]
[]
[]
[]
${{ batch_change_link }}";
    assert_eq!(
        ChangesetTemplateContext::default()
            .render_field(empty)
            .unwrap(),
        want_empty,
        "empty context"
    );
}

#[test]
fn go_struct_fields_on_the_dot() {
    let ctx = StepContext {
        repository: test_repo1(),
        steps: StepsContext {
            changes: test_changes(),
            path: "p".into(),
        },
        ..StepContext::default()
    };
    assert_eq!(
        ctx.render("${{ .Repository.Name }} ${{ .Repository.SearchResultPaths }} ${{ .Steps.Path }} ${{ .Steps.Changes.Modified }} ${{ .repository.name }}")
            .unwrap(),
        "gitlab.example.com/acme/tools README.md main.go p [go.mod] gitlab.example.com/acme/tools"
    );
}

fn partial_eval_step_ctx() -> StepContext {
    StepContext {
        batch_change: BatchChangeAttributes {
            name: "test-batch-change".into(),
            description: "test-description".into(),
        },
        // The step result is not set when a condition is evaluated.
        repository: Repository {
            name: "gitlab.example.com/acme/tools".into(),
            branch: String::new(),
            file_matches: vec!["main.go".into(), "README.md".into()],
        },
        ..StepContext::default()
    }
}

fn run_parse_and_partial(input: &str, want: &str) {
    let got = partial_eval_step_ctx()
        .partial_eval_string(input)
        .unwrap_or_else(|e| panic!("{input}: {e}"));
    assert_eq!(got, want, "{input}");
}

#[test]
fn parse_and_partial_eval_evaluated() {
    for (input, want) in [
        // Literal constants:
        (
            r#"this is my template ${{ "hardcoded string" }}"#,
            "this is my template hardcoded string",
        ),
        ("${{ 1234 }}", "1234"),
        ("${{ true }} ${{ false }}", "true false"),
        // Evaluated, since they're static values:
        (
            "${{ repository.name }} ${{ batch_change.name }} ${{ batch_change.description }}",
            "gitlab.example.com/acme/tools test-batch-change test-description",
        ),
        (
            "AAA${{ repository.name }}BBB${{ batch_change.name }}CCC${{ batch_change.description }}DDD",
            "AAAgitlab.example.com/acme/toolsBBBtest-batch-changeCCCtest-descriptionDDD",
        ),
        // Function call with static value and runtime value; aborts, since one of them is
        // a runtime value:
        (
            "${{ eq repository.name outputs.repo.name }}",
            "{{eq repository.name outputs.repo.name}}",
        ),
        // "eq" call with 2 static values:
        (
            r#"${{ eq repository.name "gitlab.example.com/acme/tools" }}"#,
            "true",
        ),
        // "eq" call with 2 literal values:
        ("${{ eq 5 5 }}", "true"),
        // "not" call:
        (r#"${{ not (eq repository.name "other-repo") }}"#, "true"),
        (
            "${{ not 1234 }} ${{ not false }} ${{ not true }}",
            "false true false",
        ),
        // "ne" call with 2 static values:
        (
            r#"${{ ne repository.name "gitlab.example.com/acme/tools" }}"#,
            "false",
        ),
        // "ne" call with 2 literal values:
        ("${{ ne 5 5 }}", "false"),
        // Function call with builtin function and static values:
        (
            r#"${{ matches repository.name "gitlab.example.com*" }}"#,
            "true",
        ),
        // Nested function call with builtin function and static values:
        (
            r#"${{ eq false (matches repository.name "gitlab.example.com*") }}"#,
            "false",
        ),
        // Nested nested function call with builtin function and static values:
        (
            r#"${{ eq false (eq false (matches repository.name "gitlab.example.com*")) }}"#,
            "true",
        ),
    ] {
        run_parse_and_partial(input, want);
    }
}

#[test]
fn parse_and_partial_eval_aborted() {
    for (input, want) in [
        // Field that doesn't exist
        (
            "${{ repository.secretlocation }}",
            "{{repository.secretlocation}}",
        ),
        // Field access that's too deep
        (
            "${{ repository.name.doesnotexist }}",
            "{{repository.name.doesnotexist}}",
        ),
        // Complex value; the string representation of templates uses standard delimiters
        (
            "${{ repository.search_result_paths }}",
            "{{repository.search_result_paths}}",
        ),
        // Runtime value
        ("${{ outputs.runtime.value }}", "{{outputs.runtime.value}}"),
        ("${{ step.modified_files }}", "{{step.modified_files}}"),
        (
            "${{ previous_step.modified_files }}",
            "{{previous_step.modified_files}}",
        ),
        // "eq" call with static value and runtime value:
        (
            "${{ eq repository.name outputs.repo.name }}",
            "{{eq repository.name outputs.repo.name}}",
        ),
        // "eq" call with more than 2 arguments:
        (
            r#"${{ eq repository.name "gitlab.example.com/acme/tools" "gitlab.example.com/acme/web" }}"#,
            r#"{{eq repository.name "gitlab.example.com/acme/tools" "gitlab.example.com/acme/web"}}"#,
        ),
        // Nested nested function call with builtin function but runtime values:
        (
            r#"${{ eq false (eq false (matches outputs.runtime.value "gitlab.example.com*")) }}"#,
            r#"{{eq false (eq false (matches outputs.runtime.value "gitlab.example.com*"))}}"#,
        ),
    ] {
        run_parse_and_partial(input, want);
    }
}

#[test]
fn parse_and_partial_eval_builtin_functions() {
    for (input, want) in [
        (
            r#"${{ join (split repository.name "/") "-" }}"#,
            "gitlab.example.com-acme-tools",
        ),
        (
            r#"${{ split repository.name "/" "-" }}"#,
            r#"{{split repository.name "/" "-"}}"#,
        ),
        (
            r#"${{ replace repository.name "gitlab" "foobar" }}"#,
            "foobar.example.com/acme/tools",
        ),
        (
            r#"${{ join_if "SEP" repository.name "postfix" }}"#,
            "gitlab.example.com/acme/toolsSEPpostfix",
        ),
        (
            r#"${{ matches repository.name "gitlab.example.com*" }}"#,
            "true",
        ),
        // Wrong argument type
        (r#"${{ join "foobar" "-" }}"#, r#"{{join "foobar" "-"}}"#),
        // Wrong argument count
        (
            r#"${{ join (split repository.name "/") "-" "too" "many" "args" }}"#,
            r#"{{join (split repository.name "/") "-" "too" "many" "args"}}"#,
        ),
    ] {
        run_parse_and_partial(input, want);
    }
}

#[test]
fn is_static_bool() {
    let ctx = partial_eval_step_ctx();
    for (name, template, want_static, want_bool) in [
        ("true literal", "true", true, true),
        ("false literal", "false", true, false),
        (
            "static non bool value",
            "${{ repository.name }}",
            true,
            false,
        ),
        (
            "function call true val",
            r#"${{ eq repository.name "gitlab.example.com/acme/tools" }}"#,
            true,
            true,
        ),
        (
            "function call false val",
            r#"${{ eq repository.name "hans wurst" }}"#,
            true,
            false,
        ),
        (
            "nested function call and whitespace",
            r#"   ${{ eq false (eq false (matches repository.name "gitlab.example.com*")) }}   "#,
            true,
            true,
        ),
        (
            "nested function call with runtime value",
            r#"${{ eq false (eq false (matches outputs.repo.name "gitlab.example.com*")) }}"#,
            false,
            false,
        ),
        ("random string", "adfadsfasdfadsfasdfasdfadsf", true, false),
    ] {
        assert_eq!(
            ctx.is_static_bool(template).unwrap(),
            (want_static, want_bool),
            "{name}"
        );
    }
    assert_eq!(
        ctx.is_static_string("x-${{ repository.name }}").unwrap(),
        Some("x-gitlab.example.com/acme/tools".to_owned())
    );
    assert_eq!(ctx.is_static_string("${{ outputs.x }}").unwrap(), None);
}
