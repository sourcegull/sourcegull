//! The step runner with the Docker runtime (copy mode) against the local Docker daemon, with
//! real `alpine` containers. Skipped when Docker is not reachable; `scripts/dev
//! test-batches-steps` runs it with the host's Docker socket.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use sourcegull_batches::BatchSpec;
use sourcegull_batches_steps::docker::{DockerOptions, DockerRuntime, WorkspaceMode};
use sourcegull_batches_steps::{
    ChangesetSpecInput, MemoryStepCache, SpecRepository, StepEvent, StepRunner, StepsError, Task,
    TaskRepository, build_changeset_specs,
};
use sourcegull_batches_template::Value;

const IMAGE: &str = "alpine:latest";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn workspace(root: &Path, name: &str) -> (std::path::PathBuf, String) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("web")).unwrap();
    std::fs::write(dir.join("README.md"), "# readme\n").unwrap();
    std::fs::write(dir.join("old.txt"), "old\n").unwrap();
    std::fs::write(dir.join("web/app.js"), "console.log(1)\n").unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    let commit = git(&dir, &["rev-parse", "HEAD"]).trim().to_owned();
    (dir, commit)
}

const SPEC: &str = r#"
name: docker-steps
description: Real containers
steps:
  - run: |
      echo "hello from $(pwd)"
      cat /tmp/greeting.txt
      echo "env=$GREETING secret=$SECRET"
      cat /tools/tool.sh
      echo "to stderr" >&2
      printf 'changed\n' >> ../README.md
      mkdir -p sub && echo new > sub/new.txt
      printf '\000\001\002' > bin.dat
      rm ../old.txt
    container: alpine:latest
    env:
      - SECRET
      - GREETING: hi ${{ repository.name }}
    files:
      /tmp/greeting.txt: greeting for ${{ batch_change.name }}
    mount:
      - path: tools
        mountpoint: /tools
    outputs:
      json:
        value: '{"files": ${{ len step.added_files }}}'
        format: json
  - run: printf '%s' "${{ previous_step.stdout }}" | head -1 > from-previous.txt
    container: alpine:latest
changesetTemplate:
  title: ${{ outputs.json.files }} files
  branch: docker-steps
  commit:
    message: m
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_steps_in_alpine() {
    let runtime = match DockerRuntime::connect(DockerOptions {
        workspace_mode: WorkspaceMode::Copy,
        ..Default::default()
    }) {
        Ok(r) if !r.version().await.is_empty() => Arc::new(r),
        _ => {
            eprintln!("skipping: Docker is not reachable");
            return;
        }
    };
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("spec");
    std::fs::create_dir_all(tools.join("tools")).unwrap();
    std::fs::write(tools.join("tools/tool.sh"), "echo tool\n").unwrap();
    let (dir, commit) = workspace(tmp.path(), "w1");
    let spec = BatchSpec::parse(SPEC).unwrap();
    let task = Task {
        spec: spec.clone(),
        repository: TaskRepository {
            name: "gitlab.example.com/g/p".into(),
            branch: "main".into(),
            base_rev: commit.clone(),
            file_matches: vec![],
        },
        path: "web".into(),
        only_fetch_workspace: false,
        global_env: [("SECRET".to_owned(), "s3cret".to_owned())].into(),
    };
    let cache = Arc::new(MemoryStepCache::new());
    let output = Arc::new(Mutex::new(String::new()));
    let sink = {
        let output = output.clone();
        move |e: &StepEvent<'_>| {
            if let StepEvent::Output { data, .. } = e {
                output
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(data));
            }
        }
    };
    let runner = StepRunner::new(runtime.clone())
        .with_cache(cache.clone())
        .with_mount_root(&tools)
        .with_log(Arc::new(sink));
    let outcome = runner.run(&task, &dir).await.unwrap();

    let first = &outcome.executed[0];
    assert_eq!(
        first.stdout,
        "hello from /work/web\ngreeting for docker-stepsenv=hi gitlab.example.com/g/p secret=s3cret\necho tool\n"
    );
    assert_eq!(first.stderr, "to stderr\n");
    assert!(output.lock().unwrap().contains("to stderr"));
    assert_eq!(first.changed_files.modified, ["README.md"]);
    assert_eq!(
        first.changed_files.added,
        ["web/bin.dat", "web/sub/new.txt"]
    );
    assert_eq!(first.changed_files.deleted, ["old.txt"]);
    assert_eq!(
        first.outputs["json"],
        Value::map([("files", Value::Float(2.0))])
    );

    // The working copy holds the changes, .git included.
    assert_eq!(
        std::fs::read_to_string(dir.join("web/from-previous.txt")).unwrap(),
        "hello from /work/web\n"
    );
    assert!(!dir.join("old.txt").exists());
    let diff = String::from_utf8_lossy(&outcome.result.diff).into_owned();
    assert!(diff.contains("GIT binary patch"), "{diff}");
    assert!(diff.contains("+++ web/from-previous.txt"), "{diff}");

    let specs = build_changeset_specs(&ChangesetSpecInput {
        spec: &spec,
        repository: &SpecRepository {
            id: "UmVwbzox".into(),
            name: task.repository.name.clone(),
            base_ref: "refs/heads/main".into(),
            base_rev: commit.clone(),
            file_matches: vec![],
        },
        path: &task.path,
        result: &outcome.result,
        fallback_author: None,
    })
    .unwrap();
    assert_eq!(specs[0].as_branch().unwrap().title, "2 files");

    // The cached diff (binary included) applies to a fresh checkout: editing the last step
    // resumes after the first, which no container runs again.
    let mut edited = task.clone();
    edited.spec.steps[1].run = "echo edited > edited.txt".into();
    let (dir2, _) = workspace(tmp.path(), "w2");
    let again = runner.run(&edited, &dir2).await.unwrap();
    assert_eq!(again.cache_hit, Some(0));
    assert_eq!(again.executed.len(), 1);
    assert_eq!(
        std::fs::read(dir2.join("web/bin.dat")).unwrap(),
        [0u8, 1, 2]
    );
    assert!(dir2.join("web/edited.txt").exists());

    // A failing step.
    let failing = Task {
        spec: BatchSpec::parse(&format!(
            "name: f\nsteps:\n  - run: echo bad >&2; exit 4\n    container: {IMAGE}\nchangesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n",
        ))
        .unwrap(),
        path: String::new(),
        ..task.clone()
    };
    let (dir3, _) = workspace(tmp.path(), "w3");
    match StepRunner::new(runtime).run(&failing, &dir3).await {
        Err(StepsError::StepFailed {
            exit_code: 4,
            stderr,
            ..
        }) => assert_eq!(stderr, "bad\n"),
        other => panic!("{other:?}"),
    }
}

/// An image running as `builder` (uid 1234), committed from alpine.
async fn non_root_image(runtime: &DockerRuntime) -> String {
    use bollard::models::{ContainerConfig, ContainerCreateBody};
    use bollard::query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder,
        RemoveContainerOptionsBuilder, WaitContainerOptionsBuilder,
    };
    use futures::StreamExt;
    use sourcegull_batches_steps::ContainerRuntime;
    let cancel = tokio_util::sync::CancellationToken::new();
    runtime.resolve_image(IMAGE, &cancel).await.unwrap();
    let docker = bollard::Docker::connect_with_defaults().unwrap();
    let name = format!("sourcegull-test-adduser-{}", std::process::id());
    docker
        .create_container(
            Some(CreateContainerOptionsBuilder::new().name(&name).build()),
            ContainerCreateBody {
                image: Some(IMAGE.into()),
                cmd: Some(vec![
                    "adduser".into(),
                    "-D".into(),
                    "-u".into(),
                    "1234".into(),
                    "builder".into(),
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker
        .start_container(
            &name,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .unwrap();
    let mut wait = docker.wait_container(
        &name,
        Some(
            WaitContainerOptionsBuilder::new()
                .condition("not-running")
                .build(),
        ),
    );
    wait.next().await.unwrap().unwrap();
    let image = "sourcegull-test-non-root";
    docker
        .commit_container(
            CommitContainerOptionsBuilder::new()
                .container(&name)
                .repo(image)
                .tag("latest")
                .build(),
            ContainerConfig {
                user: Some("builder".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker
        .remove_container(
            &name,
            Some(RemoveContainerOptionsBuilder::new().force(true).build()),
        )
        .await
        .unwrap();
    format!("{image}:latest")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_copy_belongs_to_the_image_user() {
    let runtime = match DockerRuntime::connect(DockerOptions {
        workspace_mode: WorkspaceMode::Copy,
        ..Default::default()
    }) {
        Ok(r) if !r.version().await.is_empty() => Arc::new(r),
        _ => {
            eprintln!("skipping: Docker is not reachable");
            return;
        }
    };
    let image = non_root_image(&runtime).await;
    let tmp = tempfile::tempdir().unwrap();
    let (dir, commit) = workspace(tmp.path(), "w");
    let spec = BatchSpec::parse(&format!(
        "name: owner\nsteps:\n  - run: |\n      echo \"$(id -u):$(id -g)\"\n      \
         stat -c '%u:%g' /work /work/README.md /work/web/app.js\n      \
         echo more >> README.md\n      echo new > web/created.txt\n    container: {image}\n\
         changesetTemplate:\n  title: t\n  branch: b\n  commit:\n    message: m\n"
    ))
    .unwrap();
    let task = Task {
        spec,
        repository: TaskRepository {
            name: "gitlab.example.com/g/p".into(),
            branch: "main".into(),
            base_rev: commit,
            file_matches: vec![],
        },
        path: String::new(),
        only_fetch_workspace: false,
        global_env: Default::default(),
    };
    let outcome = StepRunner::new(runtime).run(&task, &dir).await.unwrap();
    let out = &outcome.executed[0].stdout;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("1234:"), "{out}");
    assert!(lines[1..].iter().all(|l| *l == lines[0]), "{out}");
    assert_eq!(outcome.executed[0].changed_files.modified, ["README.md"]);
    assert_eq!(outcome.executed[0].changed_files.added, ["web/created.txt"]);
}
