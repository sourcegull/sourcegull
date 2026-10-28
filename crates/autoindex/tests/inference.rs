//! Inference: the built-in recognizers on path listings, override scripts, and the sandbox limits.
//!
//! Also checked here: exclusions (vendored, test and example directories, `node_modules`) take
//! effect, the .NET indexer gets the solution or project path relative to the job's root, and C/C++
//! jobs come from a committed `compile_commands.json`.

use std::collections::BTreeMap;
use std::time::Duration;

use sourcegull_autoindex::{
    DockerStep, IndexJobSpec, IndexerMap, InferenceError, InferenceResult, Limits, infer,
};

fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect()
}

fn run_with(
    entries: &[(&str, &str)],
    script: Option<&str>,
    limits: &Limits,
) -> Result<InferenceResult, InferenceError> {
    let mut f = files(entries);
    let mut r = infer(&mut f, script, &IndexerMap::default(), limits)?;
    r.jobs
        .sort_by(|a, b| (&a.indexer, &a.root).cmp(&(&b.indexer, &b.root)));
    Ok(r)
}

fn jobs(entries: &[(&str, &str)]) -> Vec<IndexJobSpec> {
    run_with(entries, None, &Limits::default()).unwrap().jobs
}

fn image(lang: &str) -> String {
    IndexerMap::default().get(lang).unwrap().to_owned()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn step(root: &str, image: &str, commands: &[&str]) -> DockerStep {
    DockerStep {
        root: root.into(),
        image: image.into(),
        commands: strings(commands),
    }
}

#[test]
fn empty_repository() {
    assert!(jobs(&[]).is_empty());
    assert!(jobs(&[("README.md", "")]).is_empty());
}

// --- Go ---

const GO_ENV: &[&str] = &[
    "GOPRIVATE",
    "GOPROXY",
    "GONOPROXY",
    "GOSUMDB",
    "GONOSUMDB",
    "NETRC_DATA",
];

#[test]
fn go_modules() {
    let got = jobs(&[("foo/bar/go.mod", ""), ("foo/baz/go.mod", "")]);
    assert_eq!(got.len(), 2);
    for (job, root) in got.iter().zip(["foo/bar", "foo/baz"]) {
        assert_eq!(job.root, root);
        assert_eq!(job.indexer, image("go"));
        assert_eq!(job.indexer_args, ["scip-go", "--no-animation"]);
        assert_eq!(job.outfile, "index.scip");
        assert_eq!(job.requested_env_vars, strings(GO_ENV));
        assert_eq!(job.steps.len(), 1);
        assert_eq!(job.steps[0].root, root);
        assert_eq!(job.steps[0].image, image("go"));
        assert!(job.steps[0].commands[0].contains("NETRC_DATA"));
        assert_eq!(job.steps[0].commands[1], "go mod download");
        assert_eq!(job.local_steps.len(), 1);
        assert!(job.local_steps[0].contains(".netrc"));
    }
}

#[test]
fn go_files_without_modules() {
    let got = jobs(&[
        ("main.go", ""),
        ("internal/a.go", ""),
        ("internal/b.go", ""),
    ]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].root, "");
    assert_eq!(
        got[0].indexer_args,
        ["GO111MODULE=off", "scip-go", "--no-animation"]
    );
    assert!(got[0].steps.is_empty());
    assert_eq!(got[0].requested_env_vars, strings(GO_ENV));
    // Only files below the root: no GOPATH project.
    assert!(jobs(&[("cmd/src/main.go", "")]).is_empty());
}

#[test]
fn go_skips_vendored_and_test_modules() {
    let got = jobs(&[
        ("go.mod", ""),
        ("vendor/github.com/x/y/go.mod", ""),
        ("internal/testdata/mod/go.mod", ""),
        ("examples/hello/go.mod", ""),
    ]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].root, "");
}

// --- JVM ---

fn java_job(root: &str) -> IndexJobSpec {
    IndexJobSpec {
        root: root.into(),
        indexer: image("java"),
        indexer_args: strings(&["scip-java", "index", "--build-tool=auto"]),
        outfile: "index.scip".into(),
        ..Default::default()
    }
}

#[test]
fn jvm_build_files_with_sources() {
    for build in [
        "lsif-java.json",
        "build.gradle",
        "build.sbt",
        "pom.xml",
        "build.gradle.kts",
    ] {
        let got = jobs(&[
            (build, ""),
            ("src/java/com/example/codeintel/dumb.java", ""),
            ("src/java/com/example/codeintel/fun.scala", ""),
        ]);
        assert_eq!(got, [java_job("")], "{build}");
    }
    // Kotlin sources count too.
    assert_eq!(
        jobs(&[("build.gradle", ""), ("app/src/Main.kt", "")]),
        [java_job("")]
    );
}

#[test]
fn jvm_without_build_file_or_sources() {
    assert!(jobs(&[("src/java/com/example/codeintel/dumb.java", "")]).is_empty());
    for build in ["pom.xml", "build.gradle", "build.sbt", "build.sc"] {
        assert!(jobs(&[(build, "")]).is_empty(), "{build}");
    }
}

#[test]
fn jvm_nested_projects() {
    // A root build covers its modules.
    assert_eq!(
        jobs(&[
            ("build.sbt", ""),
            ("my-module/src/java/com/example/codeintel/dumb.java", ""),
            ("my-module/pom.xml", ""),
        ]),
        [java_job("")]
    );
    // Without one, each module is a project.
    assert_eq!(
        jobs(&[
            ("my-module/src/java/com/example/codeintel/dumb.java", ""),
            ("my-module/pom.xml", ""),
            ("our-module/src/java/com/example/codeintel/dumb.java", ""),
            ("our-module/pom.xml", ""),
        ]),
        [java_job("my-module"), java_job("our-module")]
    );
}

// --- TypeScript / JavaScript ---

const MEM: &str =
    r#"if [ -n "${VM_MEM_MB:-}" ]; then export NODE_OPTIONS="--max-old-space-size=$VM_MEM_MB"; fi"#;
const N: &str = concat!(
    "image_node=$(node --version 2>/dev/null || true); ",
    "N_NODE_MIRROR=https://unofficial-builds.nodejs.org/download/release n --arch x64-musl auto; ",
    r#"if [ -n "$image_node" ] && ! node --version >/dev/null 2>&1; then "#,
    "N_NODE_MIRROR=https://unofficial-builds.nodejs.org/download/release ",
    r#"n --arch x64-musl "$image_node"; fi"#,
);

fn ts_job(root: &str, steps: Vec<DockerStep>, local: &[&str], infer: bool) -> IndexJobSpec {
    let mut args = vec!["scip-typescript", "index"];
    if infer {
        args.push("--infer-tsconfig");
    }
    IndexJobSpec {
        steps,
        local_steps: strings(local),
        root: root.into(),
        indexer: image("typescript"),
        indexer_args: strings(&args),
        outfile: "index.scip".into(),
        requested_env_vars: strings(&["NPM_TOKEN"]),
    }
}

#[test]
fn javascript_without_tsconfig() {
    let ts = image("typescript");
    assert_eq!(
        jobs(&[("package.json", "")]),
        [ts_job(
            "",
            vec![step("", &ts, &["npm install --ignore-scripts"])],
            &[MEM],
            true
        )]
    );
    assert_eq!(
        jobs(&[("package.json", ""), ("yarn.lock", "")]),
        [ts_job(
            "",
            vec![step("", &ts, &["yarn --ignore-scripts"])],
            &[MEM],
            true
        )]
    );
}

#[test]
fn typescript_projects() {
    assert_eq!(
        jobs(&[("tsconfig.json", "")]),
        [ts_job("", vec![], &[MEM], false)]
    );
    let got = jobs(&[
        ("a/tsconfig.json", ""),
        ("b/tsconfig.json", ""),
        ("c/tsconfig.json", ""),
    ]);
    let roots: Vec<&str> = got.iter().map(|j| j.root.as_str()).collect();
    assert_eq!(roots, ["a", "b", "c"]);
}

#[test]
fn typescript_installation_steps() {
    let ts = image("typescript");
    let got = jobs(&[
        ("tsconfig.json", ""),
        ("package.json", ""),
        ("foo/bar/package.json", ""),
        ("foo/bar/yarn.lock", ""),
        ("foo/bar/baz/tsconfig.json", ""),
        ("foo/bar/bonk/tsconfig.json", ""),
        ("foo/bar/bonk/package.json", ""),
        ("foo/baz/tsconfig.json", ""),
    ]);
    let npm_root = step("", &ts, &["npm install"]);
    let yarn_bar = step("foo/bar", &ts, &["yarn"]);
    assert_eq!(
        got,
        [
            ts_job("", vec![npm_root.clone()], &[MEM], false),
            ts_job(
                "foo/bar/baz",
                vec![npm_root.clone(), yarn_bar.clone()],
                &[MEM],
                false
            ),
            ts_job(
                "foo/bar/bonk",
                vec![
                    npm_root.clone(),
                    yarn_bar,
                    step("foo/bar/bonk", &ts, &["npm install"])
                ],
                &[MEM],
                false
            ),
            ts_job("foo/baz", vec![npm_root], &[MEM], false),
        ]
    );
}

#[test]
fn typescript_lerna_and_node_version() {
    let ts = image("typescript");
    assert_eq!(
        jobs(&[
            ("package.json", ""),
            ("lerna.json", r#"{"npmClient": "yarn"}"#),
            ("tsconfig.json", ""),
        ]),
        [ts_job("", vec![step("", &ts, &["yarn"])], &[MEM], false)]
    );
    assert_eq!(
        jobs(&[
            ("package.json", r#"{"engines": {"node": "42"}}"#),
            ("tsconfig.json", ""),
            (".nvmrc", ""),
        ]),
        [ts_job(
            "",
            vec![step("", &ts, &[N, "npm install"])],
            &[N, MEM],
            false
        )]
    );
    // A .nvmrc alone names a version too; malformed package.json files are ignored.
    assert_eq!(
        jobs(&[
            ("package.json", "{oops"),
            ("tsconfig.json", ""),
            (".nvmrc", "20")
        ])[0]
            .local_steps,
        [N, MEM]
    );
}

#[test]
fn typescript_skips_node_modules_and_tests() {
    let got = jobs(&[
        ("tsconfig.json", ""),
        ("node_modules/left-pad/tsconfig.json", ""),
        ("test/fixtures/tsconfig.json", ""),
    ]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].root, "");
}

// --- Python ---

const PKG_INFO: &str = "\nMetadata-Version: 2.1\nName: numpy\nVersion: 1.22.3\nSummary:  NumPy is the fundamental package for array computing with Python.\n";

fn py_package(root: &str, name: &str, version: &str, extra: &[&str]) -> IndexJobSpec {
    let py = image("python");
    let mut args = vec![
        "scip-python",
        "index",
        ".",
        "--project-name",
        name,
        "--project-version",
        version,
    ];
    args.extend_from_slice(extra);
    IndexJobSpec {
        steps: vec![step("", &py, &["pip install . || true"])],
        local_steps: strings(&[MEM]),
        root: root.into(),
        indexer: py,
        indexer_args: strings(&args),
        outfile: "index.scip".into(),
        ..Default::default()
    }
}

fn py_project(root: &str) -> IndexJobSpec {
    IndexJobSpec {
        local_steps: strings(&["pip install . || true", MEM]),
        root: root.into(),
        indexer: image("python"),
        indexer_args: strings(&["scip-python", "index"]),
        outfile: "index.scip".into(),
        ..Default::default()
    }
}

#[test]
fn python_packages() {
    assert_eq!(
        jobs(&[("PKG-INFO", PKG_INFO)]),
        [py_package("", "numpy", "1.22.3", &[])]
    );
    let base = PKG_INFO.replace("numpy", "numpy-base");
    assert_eq!(
        jobs(&[
            ("PKG-INFO", &base),
            ("src/numpy.egg-info/PKG-INFO", PKG_INFO)
        ]),
        [
            py_package("", "numpy-base", "1.22.3", &["--exclude", "src"]),
            py_package("src", "numpy", "1.22.3", &[]),
        ]
    );
    let proj2 = PKG_INFO
        .replace("numpy", "numpy-proj-2")
        .replace("1.22.3", "2.0.0");
    assert_eq!(
        jobs(&[
            ("PKG-INFO", &base),
            ("src/numpy.egg-info/PKG-INFO", PKG_INFO),
            ("nested/lib/proj-2.egg-info/PKG-INFO", &proj2),
        ]),
        [
            py_package("", "numpy-base", "1.22.3", &["--exclude", "nested/lib,src"]),
            py_package("nested/lib", "numpy-proj-2", "2.0.0", &[]),
            py_package("src", "numpy", "1.22.3", &[]),
        ]
    );
    // A package's bundled project files are not projects.
    assert_eq!(
        jobs(&[("PKG-INFO", PKG_INFO), ("pyproject.toml", "")]),
        [py_package("", "numpy", "1.22.3", &[])]
    );
}

#[test]
fn python_projects() {
    for file in ["pyproject.toml", "requirements.txt", "setup.py"] {
        assert_eq!(jobs(&[(file, "")]), [py_project("")], "{file}");
    }
    assert_eq!(
        jobs(&[
            ("x/pyproject.toml", ""),
            ("x/setup.py", ""),
            ("x/requirements.txt", "")
        ]),
        [py_project("x")]
    );
    assert_eq!(
        jobs(&[
            ("first_root/pyproject.toml", ""),
            ("second_root/pyproject.toml", "")
        ]),
        [py_project("first_root"), py_project("second_root")]
    );
}

// --- Ruby, Rust, .NET, C/C++ ---

#[test]
fn ruby() {
    let got = jobs(&[
        ("a/Gemfile", ""),
        ("a/a.gemspec", ""),
        ("a/Gemfile.lock", ""),
        ("a/rubygems-metadata.yml", ""),
        ("b/Gemfile", ""),
        ("c/Gemfile.lock", ""),
        ("d/d.gemspec", ""),
        ("e/rubygems-metadata.yml", ""),
    ]);
    let roots: Vec<&str> = got.iter().map(|j| j.root.as_str()).collect();
    assert_eq!(roots, ["a", "b", "c", "d", "e"]);
    assert_eq!(got[0].indexer, image("ruby"));
    assert_eq!(got[0].indexer_args, ["scip-ruby-autoindex"]);
    assert_eq!(got[0].outfile, "index.scip");
}

#[test]
fn rust() {
    assert_eq!(
        jobs(&[("foo/bar/Cargo.toml", ""), ("foo/baz/Cargo.toml", "")]),
        [IndexJobSpec {
            root: String::new(),
            indexer: image("rust"),
            indexer_args: strings(&["scip-rust", "index"]),
            outfile: "index.scip".into(),
            ..Default::default()
        }]
    );
}

fn dotnet_job(root: &str, file: &str) -> IndexJobSpec {
    IndexJobSpec {
        local_steps: strings(&["export DOTNET_EnableWriteXorExecute=0"]),
        root: root.into(),
        indexer: image("dotnet"),
        indexer_args: strings(&["scip-dotnet", "index", file, "--output", "index.scip"]),
        outfile: "index.scip".into(),
        ..Default::default()
    }
}

#[test]
fn dotnet() {
    assert_eq!(
        jobs(&[
            ("one.sln", ""),
            ("one.csproj", ""),
            ("foo/baz/two.sln", ""),
            ("foo/baz/two.vbproj", ""),
        ]),
        [dotnet_job("", "one.sln"), dotnet_job("foo/baz", "two.sln")]
    );
    assert_eq!(
        jobs(&[("one.csproj", ""), ("foo/baz/two.vbproj", "")]),
        [
            dotnet_job("", "one.csproj"),
            dotnet_job("foo/baz", "two.vbproj")
        ]
    );
}

#[test]
fn clang_needs_a_compilation_database() {
    assert!(jobs(&[("src/main.c", ""), ("CMakeLists.txt", "")]).is_empty());
    let got = jobs(&[
        ("src/main.cc", ""),
        ("build/compile_commands.json", "[]"),
        ("third_party/x/build/compile_commands.json", "[]"),
    ]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].root, "");
    assert_eq!(got[0].indexer, image("clang"));
    assert_eq!(
        got[0].indexer_args,
        ["scip-clang", "--compdb-path=build/compile_commands.json"]
    );
}

#[test]
fn several_languages_at_once() {
    let got = jobs(&[
        ("Cargo.toml", ""),
        ("ui/package.json", ""),
        ("ui/tsconfig.json", ""),
        ("tools/go.mod", ""),
    ]);
    let keys: Vec<(&str, &str)> = got
        .iter()
        .map(|j| {
            (
                sourcegull_autoindex::config::indexer_name(&j.indexer),
                j.root.as_str(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("scip-go", "tools"),
            ("scip-rust", ""),
            ("scip-typescript", "ui")
        ]
    );
}

#[test]
fn indexer_map_overrides_images() {
    let mut f = files(&[("go.mod", "")]);
    let map = IndexerMap::new([("go".to_owned(), "registry.local/scip-go:1".to_owned())].into());
    let r = infer(&mut f, None, &map, &Limits::default()).unwrap();
    assert_eq!(r.jobs[0].indexer, "registry.local/scip-go:1");
    assert_eq!(r.jobs[0].steps[0].image, "registry.local/scip-go:1");
}

// --- inference scripts ---

const CUSTOM: &str = r#"
    local path = require("path")
    local pattern = require("sg.autoindex.patterns")
    local recognizer = require("sg.autoindex.recognizer")

    local custom = recognizer.new_path_recognizer {
        patterns = { pattern.new_path_basename("acme-custom.yaml") },
        patterns_for_content = { pattern.new_path_basename("acme-custom.yaml") },
        generate = function(_, paths, contents)
            local jobs = {}
            for i = 1, #paths do
                print("found", paths[i], contents[paths[i]])
                table.insert(jobs, {
                    steps = {},
                    root = path.dirname(paths[i]),
                    indexer = "acme/custom-indexer",
                    indexer_args = { "index", contents[paths[i]] },
                    outfile = "",
                    requested_envvars = { "ACME_TOKEN" },
                })
            end
            return jobs
        end,
    }

    return require("sg.autoindex.config").new({
        ["sg.go"] = false,
        ["acme.custom"] = custom,
    })
"#;

#[test]
fn scripts_add_and_disable_recognizers() {
    let r = run_with(
        &[
            ("acme-custom.yaml", "root"),
            ("foo/acme-custom.yaml", "foo"),
            ("go.mod", ""),
            ("Cargo.toml", ""),
        ],
        Some(CUSTOM),
        &Limits::default(),
    )
    .unwrap();
    let got: Vec<(&str, &str)> = r
        .jobs
        .iter()
        .map(|j| (j.indexer.as_str(), j.root.as_str()))
        .collect();
    let rust = image("rust");
    assert_eq!(
        got,
        [
            ("acme/custom-indexer", ""),
            ("acme/custom-indexer", "foo"),
            (rust.as_str(), "")
        ]
    );
    assert_eq!(r.jobs[1].indexer_args, ["index", "foo"]);
    assert_eq!(r.jobs[1].requested_env_vars, ["ACME_TOKEN"]);
    assert!(
        r.output.contains("found\tfoo/acme-custom.yaml\tfoo\n"),
        "{}",
        r.output
    );
}

fn script_error(script: &str) -> String {
    match run_with(&[("go.mod", "")], Some(script), &Limits::default()) {
        Err(InferenceError::Script(e)) => e,
        other => panic!("expected a script error, got {other:?}"),
    }
}

#[test]
fn invalid_scripts() {
    assert!(script_error("return 42").contains("table of recognizers"));
    assert!(script_error("syntax error here").contains("syntax"));
    assert!(
        script_error(r#"return require("sg.autoindex.config").new({ ["sg.go"] = 1 })"#)
            .contains("only `sg.` prefixed")
    );
    assert!(script_error(r#"return { x = 3 }"#).contains("expected a recognizer"));
    let bad_job = r#"
        local pattern = require("sg.autoindex.patterns")
        local recognizer = require("sg.autoindex.recognizer")
        return { x = recognizer.new_path_recognizer {
            patterns = { pattern.new_path_basename("go.mod") },
            generate = function() return { { indexer = "x", rooot = "" } } end,
        } }
    "#;
    assert!(script_error(bad_job).contains("unknown field \"rooot\""));
    let no_indexer = bad_job.replace(r#"indexer = "x", rooot = """#, r#"root = "a""#);
    assert!(script_error(&no_indexer).contains("no indexer supplied"));
    assert!(script_error(r#"error("boom")"#).contains("boom"));
}

// --- the sandbox ---

fn limit_error(script: &str, limits: &Limits) -> String {
    match run_with(&[("go.mod", "")], Some(script), limits) {
        Err(InferenceError::Limit(e)) => e,
        other => panic!("expected a limit error, got {other:?}"),
    }
}

fn small() -> Limits {
    Limits {
        instructions: 2_000_000,
        timeout: Duration::from_secs(5),
        memory_bytes: 16 << 20,
        ..Limits::default()
    }
}

#[test]
fn infinite_loops_stop() {
    assert!(limit_error("while true do end", &small()).contains("instructions"));
    // pcall cannot swallow the limit.
    let swallow = "while true do pcall(function() while true do end end) end";
    assert!(limit_error(swallow, &small()).contains("instructions"));
    let nested =
        "pcall(function() while true do pcall(function() while true do end end) end end) return {}";
    assert!(limit_error(nested, &small()).contains("instructions"));
    // In a recognizer called from Rust, too.
    let in_generate = r#"
        local pattern = require("sg.autoindex.patterns")
        local recognizer = require("sg.autoindex.recognizer")
        return { spin = recognizer.new_path_recognizer {
            patterns = { pattern.new_path_basename("go.mod") },
            generate = function() while true do end end,
        } }
    "#;
    assert!(limit_error(in_generate, &small()).contains("instructions"));
    // The wall clock, when the instruction budget is large.
    let slow = Limits {
        instructions: u64::MAX,
        timeout: Duration::from_millis(200),
        ..small()
    };
    assert!(limit_error("while true do end", &slow).contains("too long"));
}

#[test]
fn memory_is_limited() {
    let blowup = "local t = {} for i = 1, 1e9 do t[i] = string.rep('x', 1024) .. i end";
    assert!(limit_error(blowup, &small()).contains("memory"));
    assert!(limit_error("local s = string.rep('x', 1 << 30)", &small()).contains("memory"));
}

#[test]
fn no_io_os_or_loading_code() {
    for (script, needle) in [
        ("return io.open('/etc/passwd')", "io"),
        ("return os.execute('id')", "os"),
        ("return os.getenv('HOME')", "os"),
        ("return require('io')", "module 'io' not found"),
        ("return require('os')", "module 'os' not found"),
        ("return load('return 1')()", "load"),
        ("return loadstring('return 1')()", "loadstring"),
        ("return dofile('/etc/passwd')", "dofile"),
        ("return loadfile('/etc/passwd')", "loadfile"),
        ("return string.dump(print)", "dump"),
        ("return debug.getinfo(1)", "debug"),
        ("return package.loadlib('x', 'y')", "package"),
        ("return coroutine.create(print)", "coroutine"),
        ("return collectgarbage('count')", "collectgarbage"),
    ] {
        let e = script_error(script);
        assert!(e.contains(needle), "{script}: {e}");
    }
    // Precompiled chunks are refused.
    let e = script_error("\x1bLua\x54\x00");
    assert!(e.contains("binary") || e.contains("mode"), "{e}");
}

#[test]
fn content_requests_are_limited() {
    let script = r#"
        local pattern = require("sg.autoindex.patterns")
        local recognizer = require("sg.autoindex.recognizer")
        return { all = recognizer.new_path_recognizer {
            patterns_for_content = { pattern.new_path_extension("txt") },
            generate = function() return {} end,
        } }
    "#;
    let many: Vec<(String, &str)> = (0..5).map(|i| (format!("{i}.txt"), "x")).collect();
    let entries: Vec<(&str, &str)> = many.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    let limits = Limits {
        max_files_with_content: 4,
        ..Limits::default()
    };
    match run_with(&entries, Some(script), &limits) {
        Err(InferenceError::Limit(e)) => assert!(e.contains("more than 4 files"), "{e}"),
        other => panic!("{other:?}"),
    }
    let limits = Limits {
        max_file_size: 0,
        ..Limits::default()
    };
    match run_with(&entries, Some(script), &limits) {
        Err(InferenceError::Limit(e)) => assert!(e.contains("larger than"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn registration_depth_is_limited() {
    let script = r#"
        local pattern = require("sg.autoindex.patterns")
        local recognizer = require("sg.autoindex.recognizer")
        local function again()
            return recognizer.new_path_recognizer {
                patterns = { pattern.new_path_basename("go.mod") },
                generate = function(api) api:register(again()) return {} end,
            }
        end
        return { loop = again() }
    "#;
    let e = limit_error(script, &Limits::default());
    assert!(e.contains("rounds"), "{e}");
}
