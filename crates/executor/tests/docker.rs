//! The executor with the Docker runtime against a real server (test Postgres, in-process repo
//! node, filesystem blob store) and the local Docker daemon. Skipped without either.
//! Uses `alpine:latest`, pulled if missing. One job reads an executor secret. The batch
//! changes scenario ([`crate::common::batches`]) runs its steps with the step runner's Docker
//! runtime.

use std::sync::Arc;
use std::time::Duration;

use crate::common::{Server, TOKEN};
use sourcegull_codeintel_scip::store::IndexJobStore;
use sourcegull_executor::docker::{DockerOptions, DockerRuntime};
use sourcegull_executor::{ExecutorConfig, Runtime};

const IMAGE: &str = "alpine:latest";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_index_jobs_in_containers() {
    let Some(server) = Server::start("127.0.0.1").await else {
        return;
    };
    let runtime = match DockerRuntime::connect(DockerOptions::default()) {
        Ok(r) if !r.version().await.is_empty() => Arc::new(r),
        _ => {
            eprintln!("skipping: Docker is not reachable");
            return;
        }
    };
    let jobs = IndexJobStore::new(&server.db);
    let c3 = server.oids.c3.clone();
    let ok = jobs
        .enqueue(&server.job(
            &c3,
            IMAGE,
            &[
                "test -f docs/more.md",
                // The commit's tag (annotated upstream) exists locally.
                "test -f .git/refs/tags/v1.0",
                "echo indexing $(pwd)",
                "printf scip-bytes > index.scip",
            ],
        ))
        .await
        .unwrap();
    let failing = jobs
        .enqueue(&server.job(&c3, IMAGE, &["echo about to fail", "exit 3"]))
        .await
        .unwrap();
    let no_index = jobs
        .enqueue(&server.job(&server.oids.c1, IMAGE, &["true"]))
        .await
        .unwrap();
    let secret = jobs.enqueue(&server.secret_job(&c3, IMAGE)).await.unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let name = format!("test-executor-{}", uuid::Uuid::new_v4().simple());
    let executor = tokio::spawn(sourcegull_executor::run(
        ExecutorConfig {
            url: format!("http://127.0.0.1:{}", server.port).parse().unwrap(),
            token: TOKEN.into(),
            name: name.clone(),
            queues: vec!["codeintel".into()],
            max_jobs: 2,
            poll_interval: Duration::from_millis(200),
            heartbeat_interval: Duration::from_secs(1),
            scratch_dir: scratch.path().to_owned(),
            git: "git".into(),
        },
        runtime,
        server.stop.clone(),
    ));

    let rows = server
        .wait(&[ok, failing, no_index, secret], Duration::from_secs(300))
        .await;
    server
        .assert_uploaded(
            &rows[0],
            &[
                "setup.git.fetch",
                "setup.git.checkout",
                "step.docker.indexer",
                "step.upload",
            ],
            b"scip-bytes",
        )
        .await;
    Server::assert_step_failed(&rows[1]);
    assert_eq!(rows[2].state, "failed");
    assert_eq!(
        rows[2].failure_message.as_deref(),
        Some("the indexer wrote no index.scip")
    );
    server.assert_secret_job(&rows[3]).await;

    // The executor reported itself, and left no containers or volumes behind.
    server.assert_executor(&name, "docker").await;
    server.stop.cancel();
    executor.await.unwrap().unwrap();
    assert_nothing_left(&name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_batches_jobs_in_containers() {
    let Some(server) = Server::start("127.0.0.1").await else {
        return;
    };
    let runtime = match DockerRuntime::connect(DockerOptions::default()) {
        Ok(r) if !r.version().await.is_empty() => Arc::new(r),
        _ => {
            eprintln!("skipping: Docker is not reachable");
            return;
        }
    };
    let name =
        crate::common::batches::run_scenario(&server, runtime, Duration::from_secs(300)).await;
    assert_nothing_left(&name);
}

/// No container or volume of the executor `name` is left.
fn assert_nothing_left(name: &str) {
    let filter = format!("label=io.sourcegull.executor={name}");
    for args in [
        vec!["volume", "ls", "-q", "--filter", &filter],
        vec!["container", "ls", "-a", "-q", "--filter", &filter],
    ] {
        let out = std::process::Command::new("docker").args(&args).output();
        if let Ok(out) = out
            && out.status.success()
        {
            let left = String::from_utf8_lossy(&out.stdout);
            assert!(left.trim().is_empty(), "{} left behind: {left}", args[0]);
        }
    }
}
