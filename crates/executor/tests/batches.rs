//! The executor running `batches` jobs against a real server (test Postgres, in-process repo node, the HTTP API with auto-apply) with the step runner's
//! in-process fake runtime instead of containers: see [`crate::common::batches`] for the scenario.
//! Skipped without a test database.

use std::sync::Arc;
use std::time::Duration;

use crate::common::Server;
use async_trait::async_trait;
use sourcegull_batches_steps::fake::{FakeOutput, FakeRuntime};
use sourcegull_batches_steps::runtime::OutputFn;
use sourcegull_batches_steps::{ContainerRuntime, ContainerSpec, RuntimeError, WORKSPACE_DIR};
use sourcegull_executor::batches::StepsRuntime;
use tokio_util::sync::CancellationToken;

/// Plays the scenario's step scripts line by line, on the host working copy.
fn play(spec: &ContainerSpec) -> FakeOutput {
    let rel = spec
        .workdir
        .trim_start_matches(WORKSPACE_DIR)
        .trim_start_matches('/');
    let dir = spec.workspace.join(rel);
    let mut out = FakeOutput::ok();
    for line in spec.script.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line == "echo \"token is $SOURCEGULL_BATCHES_SECRET\"" {
            let token = spec
                .env
                .get("SOURCEGULL_BATCHES_SECRET")
                .cloned()
                .unwrap_or_default();
            out.stdout.push_str(&format!("token is {token}\n"));
        } else if line.starts_with("test -d /work/src") {
            out.stdout.push_str(if spec.workspace.join("src").is_dir() {
                "src checked out\n"
            } else {
                "src not checked out\n"
            });
        } else if let Some(text) = line
            .strip_suffix(" >&2")
            .and_then(|l| l.strip_prefix("echo "))
        {
            out.stderr.push_str(&format!("{text}\n"));
        } else if let Some((text, file)) = line
            .strip_prefix("echo ")
            .and_then(|l| l.split_once(" >> "))
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join(file))
                .unwrap();
            writeln!(f, "{text}").unwrap();
        } else if let Some((source, file)) =
            line.strip_prefix("cat ").and_then(|l| l.split_once(" >> "))
        {
            let mount = spec
                .mounts
                .iter()
                .find(|m| m.target == source)
                .unwrap_or_else(|| panic!("{source} is not mounted"));
            let content = std::fs::read(&mount.source).unwrap();
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.join(file))
                .unwrap();
            f.write_all(&content).unwrap();
        } else if let Some(code) = line.strip_prefix("exit ") {
            out.exit_code = code.parse().unwrap();
            return out;
        } else {
            panic!("the fake cannot run {line:?}");
        }
    }
    out
}

/// The fake, except that `sleep` runs until the step is canceled.
struct Steps(FakeRuntime);

#[async_trait]
impl ContainerRuntime for Steps {
    async fn resolve_image(
        &self,
        image: &str,
        cancel: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        self.0.resolve_image(image, cancel).await
    }

    async fn run(
        &self,
        spec: &ContainerSpec,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        if spec.script.starts_with("sleep") {
            tokio::select! {
                _ = cancel.cancelled() => return Err(RuntimeError::Canceled),
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    return Err(RuntimeError::Other("not canceled in time".into()));
                }
            }
        }
        self.0.run(spec, output, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_batches_jobs_with_the_step_runner() {
    let Some(server) = Server::start("127.0.0.1").await else {
        return;
    };
    let runtime = Arc::new(StepsRuntime::new(
        Arc::new(Steps(FakeRuntime::new(play))),
        "fake",
    ));
    crate::common::batches::run_scenario(&server, runtime, Duration::from_secs(60)).await;
}
