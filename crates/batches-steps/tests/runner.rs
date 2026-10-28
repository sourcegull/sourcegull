//! The step runner with the in-process fake runtime, on real git working copies.
//!
//! The fake containers interpret a tiny script language (see [`interpret`]) so that steps can
//! change files, print and fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use sourcegull_batches::{BatchSpec, ChangesetSpec, PublishedValue};
use sourcegull_batches_steps::fake::{FakeOutput, FakeRuntime};
use sourcegull_batches_steps::{
    AfterStepResult, CachedStep, ChangesetSpecInput, ContainerSpec, MemoryStepCache, OutputStream,
    SpecRepository, StepCache, StepEvent, StepRunner, StepsError, Task, TaskRepository,
    WORKSPACE_DIR, build_changeset_specs,
};
use sourcegull_batches_template::Value;

const REPO: &str = "gitlab.example.com/group/project";

/// Runs the fake script: one command per line.
/// `write PATH TEXT`, `append PATH TEXT`, `rm PATH`, `mv FROM TO`, `echo TEXT` (stdout),
/// `err TEXT` (stderr), `exit N`; paths are relative to the working directory.
fn interpret(spec: &ContainerSpec) -> FakeOutput {
    let rel = spec.workdir.strip_prefix(WORKSPACE_DIR).unwrap_or_default();
    let dir = spec.workspace.join(rel.trim_start_matches('/'));
    let mut out = FakeOutput::ok();
    for line in spec.script.lines() {
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let (a, b) = rest.split_once(' ').unwrap_or((rest, ""));
        match cmd {
            "write" => {
                let p = dir.join(a);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, format!("{b}\n")).unwrap();
            }
            "append" => {
                let p = dir.join(a);
                let mut text = std::fs::read_to_string(&p).unwrap_or_default();
                text.push_str(b);
                text.push('\n');
                std::fs::write(p, text).unwrap();
            }
            "rm" => std::fs::remove_file(dir.join(a)).unwrap(),
            "mv" => std::fs::rename(dir.join(a), dir.join(b)).unwrap(),
            "echo" => {
                out.stdout.push_str(rest);
                out.stdout.push('\n');
            }
            "err" => {
                out.stderr.push_str(rest);
                out.stderr.push('\n');
            }
            "exit" => {
                out.exit_code = rest.parse().unwrap();
                return out;
            }
            "" => {}
            other => panic!("unknown fake command {other:?}"),
        }
    }
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A working copy with a few files committed; returns the directory and the commit.
fn workspace(root: &Path, name: &str) -> (PathBuf, String) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::write(dir.join("README.md"), "# readme\n").unwrap();
    std::fs::write(dir.join("old.txt"), "old\n").unwrap();
    std::fs::write(dir.join("keep.txt"), "a file that will be moved\n").unwrap();
    std::fs::write(dir.join("a/x.txt"), "x\n").unwrap();
    std::fs::write(dir.join("b/y.txt"), "y\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    let commit = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    (dir, commit)
}

/// Parses a spec, adding a minimal `changesetTemplate` when it has none (steps need one).
fn spec(yaml: &str) -> BatchSpec {
    let mut yaml = yaml.to_owned();
    if !yaml.contains("changesetTemplate") {
        yaml.push_str("\nchangesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n");
    }
    BatchSpec::parse(&yaml).unwrap_or_else(|e| panic!("{e}"))
}

fn task(spec: BatchSpec, commit: &str) -> Task {
    Task {
        spec,
        repository: TaskRepository {
            name: REPO.into(),
            branch: "main".into(),
            base_rev: commit.into(),
            file_matches: vec!["b/y.txt".into(), "a/x.txt".into()],
        },
        path: String::new(),
        only_fetch_workspace: false,
        global_env: [("SECRET".to_owned(), "s3cret".to_owned())].into(),
    }
}

fn repo(commit: &str) -> SpecRepository {
    SpecRepository {
        id: "UmVwb3NpdG9yeTox".into(),
        name: REPO.into(),
        base_ref: "refs/heads/main".into(),
        base_rev: commit.into(),
        file_matches: vec!["a/x.txt".into()],
    }
}

const TEMPLATE: &str = r#"
changesetTemplate:
  title: Update ${{ outputs.greeting }}
  body: Changed ${{ join steps.modified_files ", " }} in ${{ repository.name }}
  branch: batch/${{ batch_change.name }}
  commit:
    message: ${{ batch_change.description }}
"#;

#[tokio::test]
async fn if_outputs_previous_step_and_file_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, commit) = workspace(tmp.path(), "w");
    let s = spec(&format!(
        r#"
name: hello
description: Say hello
steps:
  - run: |
      echo hi
      err warning
      append README.md more
      write new.txt new
    container: alpine:3
    outputs:
      greeting:
        value: ${{{{ step.stdout }}}}
      config:
        value: '{{"n": 3, "list": ["a", "b"]}}'
        format: json
      meta:
        value: |
          count: 1
          ratio: 0.5
          from: ${{{{ outputs.greeting }}}}
        format: yaml
  - run: echo never
    container: alpine:3
    if: ${{{{ eq outputs.greeting "nope" }}}}
  - run: |
      echo prev=${{{{ previous_step.stdout }}}}
      echo mod=${{{{ join steps.modified_files "," }}}} added=${{{{ join previous_step.added_files "," }}}}
      echo n=${{{{ outputs.config.n }}}} count=${{{{ outputs.meta.count }}}} list=${{{{ join outputs.config.list "+" }}}}
      echo paths=${{{{ repository.search_result_paths }}}} path=[${{{{ steps.path }}}}]
      rm old.txt
      mv keep.txt moved.txt
    container: busybox
    if: ${{{{ eq outputs.greeting "hi\n" }}}}
    env:
      - SECRET
      - REPO: ${{{{ repository.name }}}}
    files:
      /tmp/name.txt: ${{{{ batch_change.name }}}} in ${{{{ repository.branch }}}}
    outputs:
      last:
        value: ${{{{ join step.deleted_files "," }}}}|${{{{ join step.renamed_files "," }}}}|${{{{ step.stdout }}}}
{TEMPLATE}"#
    ));
    let runtime = Arc::new(FakeRuntime::new(interpret));
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let events = events.clone();
        move |e: &StepEvent<'_>| {
            let s = match e {
                StepEvent::Output { step, stream, data } => format!(
                    "{step} {}: {}",
                    if *stream == OutputStream::Stdout {
                        "out"
                    } else {
                        "err"
                    },
                    String::from_utf8_lossy(data)
                ),
                StepEvent::Skipped { step } => format!("{step} skipped"),
                StepEvent::Finished { step, exit_code } => format!("{step} exit {exit_code}"),
                _ => return,
            };
            events.lock().unwrap().push(s);
        }
    };
    let runner = StepRunner::new(runtime.clone()).with_log(Arc::new(sink));
    let t = task(s.clone(), &commit);
    let outcome = runner.run(&t, &dir).await.unwrap();

    assert_eq!(outcome.skipped, [1]);
    assert_eq!(outcome.executed.len(), 2);
    assert_eq!(outcome.cache_hit, None);
    let runs = runtime.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runtime.resolved(), ["alpine:3", "busybox"]);
    assert!(runs[0].digest.starts_with("sha256:"));
    assert_eq!(runs[1].workdir, "/work");
    let script = &runs[1].script;
    assert!(script.contains("echo prev=hi\n"), "{script}");
    assert!(
        script.contains("echo mod=README.md added=new.txt\n"),
        "{script}"
    );
    assert!(script.contains("echo n=3 count=1 list=a+b\n"), "{script}");
    assert!(
        script.contains("echo paths=a/x.txt b/y.txt path=[]\n"),
        "{script}"
    );
    assert_eq!(runs[1].env["SECRET"], "s3cret");
    assert_eq!(runs[1].env["REPO"], REPO);
    assert_eq!(runs[1].files[0].target, "/tmp/name.txt");
    assert_eq!(runs[1].files[0].content, b"hello in main");

    let first = &outcome.executed[0];
    assert_eq!(first.stdout, "hi\n");
    assert_eq!(first.stderr, "warning\n");
    assert_eq!(first.changed_files.modified, ["README.md"]);
    assert_eq!(first.changed_files.added, ["new.txt"]);
    assert_eq!(first.outputs["greeting"], Value::from("hi\n"));
    assert_eq!(
        first.outputs["meta"],
        Value::map([
            ("count", Value::Int(1)),
            ("ratio", Value::Float(0.5)),
            ("from", Value::from("hi")),
        ])
    );

    let last = &outcome.result;
    assert_eq!(last.step_index, 2);
    assert_eq!(last.changed_files.modified, ["README.md"]);
    assert_eq!(last.changed_files.added, ["new.txt"]);
    assert_eq!(last.changed_files.deleted, ["old.txt"]);
    assert_eq!(last.changed_files.renamed, ["moved.txt"]);
    let Value::String(out) = &last.outputs["last"] else {
        panic!()
    };
    assert!(out.starts_with("old.txt|moved.txt|prev=hi\n"), "{out}");
    let diff = String::from_utf8(last.diff.clone()).unwrap();
    assert!(diff.contains("diff --git README.md README.md\n"), "{diff}");
    assert!(diff.contains("+++ new.txt\n"), "{diff}");
    assert!(
        diff.contains("rename from keep.txt\nrename to moved.txt\n"),
        "{diff}"
    );
    assert!(diff.contains("+++ /dev/null"), "{diff}");
    // The first step's diff is its own snapshot.
    assert!(!String::from_utf8_lossy(&first.diff).contains("old.txt"));

    let events = events.lock().unwrap().clone();
    assert_eq!(
        events,
        [
            "0 out: hi\n",
            "0 err: warning\n",
            "0 exit 0",
            "1 skipped",
            "2 out: prev=hi\nmod=README.md added=new.txt\nn=3 count=1 list=a+b\npaths=a/x.txt b/y.txt path=[]\n",
            "2 exit 0",
        ]
    );

    let specs = build_changeset_specs(&ChangesetSpecInput {
        spec: &s,
        repository: &repo(&commit),
        path: "",
        result: last,
        fallback_author: None,
    })
    .unwrap();
    assert_eq!(specs.len(), 1);
    let b = specs[0].as_branch().unwrap();
    assert_eq!(b.title, "Update hi");
    assert_eq!(b.body, format!("Changed README.md in {REPO}"));
    assert_eq!(b.head_ref, "refs/heads/batch/hello");
    assert_eq!(b.base_ref, "refs/heads/main");
    assert_eq!(b.base_rev, commit);
    assert_eq!(b.published, None);
    let c = b.commit().unwrap();
    assert_eq!(c.message, "Say hello");
    assert_eq!(c.author_name, "Sourcegull");
    assert_eq!(c.diff, last.diff);
    // The JSON is what `createChangesetSpec` accepts.
    assert_eq!(ChangesetSpec::parse(&specs[0].to_json()).unwrap(), specs[0]);
}

#[tokio::test]
async fn workspace_paths_failures_and_output_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, commit) = workspace(tmp.path(), "w");
    let s = spec(
        r#"
name: fail
steps:
  - run: |
      write in-a.txt a
      echo partial
      err broken
      exit 3
    container: alpine:3
"#,
    );
    let runtime = Arc::new(FakeRuntime::new(interpret));
    let runner = StepRunner::new(runtime.clone());
    let mut t = task(s, &commit);
    t.path = "a".into();
    match runner.run(&t, &dir).await {
        Err(StepsError::StepFailed {
            step: 0,
            exit_code: 3,
            stdout,
            stderr,
        }) => {
            assert_eq!(stdout, "partial\n");
            assert_eq!(stderr, "broken\n");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(runtime.runs()[0].workdir, "/work/a");
    assert!(dir.join("a/in-a.txt").exists());

    let bad_json = spec(
        r#"
name: bad
steps:
  - run: echo x
    container: alpine:3
    outputs:
      v:
        value: "{ not json"
        format: json
"#,
    );
    let (dir2, commit2) = workspace(tmp.path(), "w2");
    let err = runner
        .run(&task(bad_json, &commit2), &dir2)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StepsError::Output { step: 0, name, .. } if name == "v"),
        "{err:?}"
    );

    let bad_template = spec(
        r#"
name: bad
steps:
  - run: echo ${{ repository.nope }}
    container: alpine:3
"#,
    );
    let err = runner
        .run(&task(bad_template, &commit2), &dir2)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StepsError::Template { step: 0, field, .. } if field == "run"),
        "{err:?}"
    );

    runtime.set_missing("ghost:1");
    let missing = spec("name: m\nsteps:\n  - run: echo\n    container: ghost:1\n");
    assert!(matches!(
        runner.run(&task(missing, &commit2), &dir2).await,
        Err(StepsError::Image(_))
    ));
}

#[tokio::test]
async fn transform_changes_groups_and_published() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, commit) = workspace(tmp.path(), "w");
    let s = spec(
        r#"
name: groups
description: Grouped
steps:
  - run: |
      append a/x.txt more
      append b/y.txt more
      append README.md more
      write a/b/deep.txt deep
    container: alpine:3
    outputs:
      name:
        value: things
transformChanges:
  group:
    - directory: a
      branch: group-a
    - directory: a/b
      branch: group-ab
    - directory: b
      branch: group-b
      repository: gitlab.example.com/other/repo
changesetTemplate:
  title: Update ${{ outputs.name }}
  body: body
  branch: batch/main-change
  commit:
    message: msg
    author:
      name: ${{ batch_change.name }} bot
      email: bot@example.com
  published:
    - "gitlab.example.com/*": draft
    - "gitlab.example.com/group/project@group-a": true
"#,
    );
    let runtime = Arc::new(FakeRuntime::new(interpret));
    let outcome = StepRunner::new(runtime)
        .run(&task(s.clone(), &commit), &dir)
        .await
        .unwrap();
    let specs = build_changeset_specs(&ChangesetSpecInput {
        spec: &s,
        repository: &repo(&commit),
        path: "",
        result: &outcome.result,
        fallback_author: None,
    })
    .unwrap();
    let got: Vec<(String, Vec<String>, Option<PublishedValue>)> = specs
        .iter()
        .map(|s| {
            let b = s.as_branch().unwrap();
            let diff = String::from_utf8(b.commit().unwrap().diff.clone()).unwrap();
            let files = diff
                .lines()
                .filter_map(|l| l.strip_prefix("diff --git "))
                .map(|l| l.split(' ').next().unwrap().to_owned())
                .collect();
            (b.head_branch().to_owned(), files, b.published)
        })
        .collect();
    // `b` belongs to another repository's group; `a/b/deep.txt` matches both `a` and `a/b`,
    // and the last group wins.
    assert_eq!(
        got,
        [
            (
                "batch/main-change".to_owned(),
                vec!["README.md".to_owned(), "b/y.txt".to_owned()],
                Some(PublishedValue::Draft)
            ),
            (
                "group-a".to_owned(),
                vec!["a/x.txt".to_owned()],
                Some(PublishedValue::Published)
            ),
            (
                "group-ab".to_owned(),
                vec!["a/b/deep.txt".to_owned()],
                Some(PublishedValue::Draft)
            ),
        ]
    );
    let b = specs[0].as_branch().unwrap();
    assert_eq!(b.title, "Update things");
    assert_eq!(b.commit().unwrap().author_name, "groups bot");
    for s in &specs {
        assert_eq!(&ChangesetSpec::parse(&s.to_json()).unwrap(), s);
    }

    // A group branch equal to the template branch is an error.
    let mut clash = s.clone();
    clash.transform_changes.as_mut().unwrap().group[0].branch = "batch/main-change".into();
    let err = build_changeset_specs(&ChangesetSpecInput {
        spec: &clash,
        repository: &repo(&commit),
        path: "",
        result: &outcome.result,
        fallback_author: None,
    })
    .unwrap_err();
    assert!(matches!(err, StepsError::TransformChanges(_)), "{err:?}");
}

const CACHED: &str = r#"
name: cached
steps:
  - run: |
      append log.txt one
      echo first
    container: alpine:3
    outputs:
      first:
        value: ${{ step.stdout }}
  - run: append log.txt two
    container: alpine:3
    env:
      - SECRET
  - run: append log.txt three ${{ outputs.first }}
    container: busybox
changesetTemplate:
  title: t ${{ outputs.first }}
  branch: b
  commit:
    message: m
"#;

#[tokio::test]
async fn cache_resumes_after_the_last_unchanged_step() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Arc::new(MemoryStepCache::new());
    let runtime = Arc::new(FakeRuntime::new(interpret));
    let runner = StepRunner::new(runtime.clone()).with_cache(cache.clone());

    let (dir, commit) = workspace(tmp.path(), "w1");
    let s = spec(CACHED);
    let first = runner.run(&task(s.clone(), &commit), &dir).await.unwrap();
    assert_eq!(first.executed.len(), 3);
    assert_eq!(cache.len(), 3);
    let log = std::fs::read_to_string(dir.join("log.txt")).unwrap();
    assert_eq!(log, "one\ntwo\nthree first\n");

    // Edit step 2 (index 1): step 1 comes from the cache, steps 2 and 3 run again on top of
    // its diff, with its outputs.
    let edited = spec(&CACHED.replace("append log.txt two", "append log.txt TWO"));
    let (dir2, _) = workspace(tmp.path(), "w2");
    let before = runtime.runs().len();
    let second = runner
        .run(&task(edited.clone(), &commit), &dir2)
        .await
        .unwrap();
    assert_eq!(second.cache_hit, Some(0));
    let runs = runtime.runs()[before..].to_vec();
    assert_eq!(
        runs.iter().map(|r| r.step).collect::<Vec<_>>(),
        [1, 2],
        "only the edited step and later ones run"
    );
    assert_eq!(runs[1].script, "append log.txt three first\n");
    assert_eq!(
        std::fs::read_to_string(dir2.join("log.txt")).unwrap(),
        "one\nTWO\nthree first\n"
    );
    assert_eq!(second.result.changed_files.added, ["log.txt"]);

    // A new secret value invalidates step 2 and later; a moved tag everything.
    let (dir3, _) = workspace(tmp.path(), "w3");
    let mut t = task(edited.clone(), &commit);
    t.global_env.insert("SECRET".into(), "rotated".into());
    let before = runtime.runs().len();
    let third = runner.run(&t, &dir3).await.unwrap();
    assert_eq!(third.cache_hit, Some(0));
    assert_eq!(runtime.runs().len() - before, 2);

    runtime.set_digest("alpine:3", "sha256:moved");
    let (dir4, _) = workspace(tmp.path(), "w4");
    let before = runtime.runs().len();
    let fourth = runner.run(&t, &dir4).await.unwrap();
    assert_eq!(fourth.cache_hit, None);
    assert_eq!(runtime.runs().len() - before, 3);
    assert_eq!(fourth.result.diff, third.result.diff);

    // A different base commit misses.
    let (dir5, _) = workspace(tmp.path(), "w5");
    let before = runtime.runs().len();
    runner.run(&task(edited, "0000"), &dir5).await.unwrap();
    assert_eq!(runtime.runs().len() - before, 3);
}

#[tokio::test]
async fn fully_cached_workspaces_need_no_container() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = Arc::new(MemoryStepCache::new());
    let (dir, commit) = workspace(tmp.path(), "w1");
    let s = spec(CACHED);
    let t = task(s.clone(), &commit);
    let first = StepRunner::new(Arc::new(FakeRuntime::new(interpret)))
        .with_cache(cache.clone())
        .run(&t, &dir)
        .await
        .unwrap();

    // Every step cached: nothing runs and the working copy is not touched.
    let idle = Arc::new(FakeRuntime::new(|_| panic!("no container should run")));
    let (dir2, _) = workspace(tmp.path(), "w2");
    let again = StepRunner::new(idle.clone())
        .with_cache(cache.clone())
        .run(&t, &dir2)
        .await
        .unwrap();
    assert!(idle.runs().is_empty());
    assert_eq!(again.cache_hit, Some(2));
    assert!(again.executed.is_empty());
    assert_eq!(again.result, first.result);
    assert!(!dir2.join("log.txt").exists());

    // Specs from the stored entry alone (what a server does with `cachedResultFound`).
    let keys = sourcegull_batches_steps::step_cache_keys(
        &t,
        &[Default::default(), Default::default(), Default::default()],
    );
    let stored = cache.get(&keys[2].key).await.unwrap().unwrap();
    let stored = CachedStep::from_json(&stored.to_json()).unwrap();
    let specs = build_changeset_specs(&ChangesetSpecInput {
        spec: &s,
        repository: &repo(&commit),
        path: "",
        result: &stored.result,
        fallback_author: None,
    })
    .unwrap();
    assert_eq!(specs.len(), 1);
    let b = specs[0].as_branch().unwrap();
    assert_eq!(b.title, "t first");
    assert_eq!(b.commit().unwrap().diff, first.result.diff);
    // Its fingerprint is not that of default inputs.
    assert_ne!(stored.fingerprint, keys[2].fingerprint);

    // Nothing to change: no specs.
    let empty = AfterStepResult::default();
    assert!(
        build_changeset_specs(&ChangesetSpecInput {
            spec: &s,
            repository: &repo(&commit),
            path: "",
            result: &empty,
            fallback_author: None,
        })
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn skipped_last_step_and_mounts() {
    let tmp = tempfile::tempdir().unwrap();
    let mounts = tmp.path().join("spec-dir");
    std::fs::create_dir_all(mounts.join("tools")).unwrap();
    std::fs::write(mounts.join("tools/fix.sh"), "v1").unwrap();
    let s = spec(
        r#"
name: mounts
steps:
  - run: append log.txt one
    container: alpine:3
    mount:
      - path: ./tools
        mountpoint: /tools
  - run: append log.txt never
    container: alpine:3
    if: false
"#,
    );
    let cache = Arc::new(MemoryStepCache::new());
    let runtime = Arc::new(FakeRuntime::new(interpret));
    let runner = StepRunner::new(runtime.clone())
        .with_cache(cache.clone())
        .with_mount_root(&mounts);
    let (dir, commit) = workspace(tmp.path(), "w1");
    let t = task(s.clone(), &commit);
    let first = runner.run(&t, &dir).await.unwrap();
    assert_eq!(first.skipped, [1]);
    assert_eq!(first.result.step_index, 0);
    let run = &runtime.runs()[0];
    assert_eq!(run.mounts[0].source, mounts.join("./tools"));
    assert_eq!(run.mounts[0].target, "/tools");

    // The skipped last step has no entry; the run resumes after step 1 and skips again.
    let (dir2, _) = workspace(tmp.path(), "w2");
    let second = runner.run(&t, &dir2).await.unwrap();
    assert_eq!(second.cache_hit, Some(0));
    assert_eq!(runtime.runs().len(), 1);
    assert_eq!(second.result.diff, first.result.diff);

    // Changed mount contents run the step again.
    std::fs::write(mounts.join("tools/fix.sh"), "v2").unwrap();
    let (dir3, _) = workspace(tmp.path(), "w3");
    let third = runner.run(&t, &dir3).await.unwrap();
    assert_eq!(third.cache_hit, None);
    assert_eq!(runtime.runs().len(), 2);

    // Mounts must stay in the mount root, and need one.
    let escape = spec(
        "name: m\nsteps:\n  - run: x\n    container: alpine:3\n    mount:\n      - path: ../secrets\n        mountpoint: /s\n",
    );
    let (dir4, _) = workspace(tmp.path(), "w4");
    assert!(matches!(
        runner.run(&task(escape, &commit), &dir4).await,
        Err(StepsError::InvalidPath { step: 0, .. })
    ));
    assert!(matches!(
        StepRunner::new(runtime.clone()).run(&t, &dir4).await,
        Err(StepsError::InvalidPath { step: 0, .. })
    ));
}
