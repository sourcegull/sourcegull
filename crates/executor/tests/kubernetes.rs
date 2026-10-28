//! The executor with the Kubernetes runtime against a real server and a real cluster
//! (`scripts/dev test-kubernetes` starts k3s). Skipped unless `SOURCEGULL_TEST_KUBERNETES_POD_HOST`
//! names this machine's address as job pods reach it; the cluster comes from `KUBECONFIG`.
//! Uses `alpine:3.22` for steps and `alpine/git:v2.52.0` as the helper image. The batch
//! changes scenario ([`crate::common::batches`]) runs its steps as Kubernetes Jobs too.

use std::sync::Arc;
use std::time::Duration;

use crate::common::{Server, TOKEN};
use k8s_openapi::api::batch::v1::Job as KubeJob;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use sourcegull_codeintel_scip::store::IndexJobStore;
use sourcegull_executor::kubernetes::{KubernetesOptions, KubernetesRuntime};
use sourcegull_executor::{ExecutorConfig, Runtime};

const IMAGE: &str = "alpine:3.22";
const HELPER: &str = "alpine/git:v2.52.0";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_index_jobs_as_kubernetes_jobs() {
    let Ok(pod_host) = std::env::var("SOURCEGULL_TEST_KUBERNETES_POD_HOST") else {
        eprintln!("skipping: SOURCEGULL_TEST_KUBERNETES_POD_HOST is not set");
        return;
    };
    let Some(server) = Server::start("0.0.0.0").await else {
        return;
    };
    let client = kube::Client::try_default().await.expect("a cluster");
    let namespace = format!(
        "sourcegull-test-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(serde_json::json!({
                "metadata": { "name": namespace }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let runtime = Arc::new(KubernetesRuntime::with_client(
        client.clone(),
        KubernetesOptions {
            namespace: namespace.clone(),
            helper_image: HELPER.into(),
            pod_frontend_url: Some(
                format!("http://{pod_host}:{}", server.port)
                    .parse()
                    .unwrap(),
            ),
            memory_limit: Some("256Mi".into()),
            start_timeout: Duration::from_secs(180),
            ..Default::default()
        },
    ));
    assert!(!runtime.version().await.is_empty());

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
    let bad_image = jobs
        .enqueue(&server.job(&c3, "sourcegull.invalid/no-such-image:1", &["true"]))
        .await
        .unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let name = format!("test-executor-{}", uuid::Uuid::new_v4().simple());
    let executor = tokio::spawn(sourcegull_executor::run(
        ExecutorConfig {
            url: format!("http://127.0.0.1:{}", server.port).parse().unwrap(),
            token: TOKEN.into(),
            name: name.clone(),
            queues: vec!["codeintel".into()],
            max_jobs: 4,
            poll_interval: Duration::from_millis(200),
            heartbeat_interval: Duration::from_secs(1),
            scratch_dir: scratch.path().to_owned(),
            git: "git".into(),
        },
        runtime,
        server.stop.clone(),
    ));

    let rows = server
        .wait(
            &[ok, failing, no_index, bad_image, secret],
            Duration::from_secs(600),
        )
        .await;
    server
        .assert_uploaded(
            &rows[0],
            &["setup.git", "step.docker.indexer", "step.upload"],
            b"scip-bytes",
        )
        .await;
    let checkout = rows[0]
        .execution_logs
        .iter()
        .find(|e| e.key == "setup.git")
        .unwrap();
    assert_eq!(checkout.exit_code, Some(0));
    assert!(checkout.out.contains("+ git checkout"), "{}", checkout.out);
    Server::assert_step_failed(&rows[1]);
    assert_eq!(rows[2].state, "failed");
    assert_eq!(
        rows[2].failure_message.as_deref(),
        Some("the indexer wrote no index.scip")
    );
    assert_eq!(rows[3].state, "failed");
    let message = rows[3].failure_message.as_deref().unwrap_or_default();
    assert!(
        message.starts_with("pulling sourcegull.invalid/no-such-image:1"),
        "{message}"
    );
    server.assert_secret_job(&rows[4]).await;

    server.assert_executor(&name, "kubernetes").await;
    server.stop.cancel();
    executor.await.unwrap().unwrap();

    // Nothing is left behind.
    let labels = ListParams::default().labels(&format!("io.sourcegull.executor={name}"));
    let kube_jobs: Api<KubeJob> = Api::namespaced(client.clone(), &namespace);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    assert!(kube_jobs.list(&labels).await.unwrap().items.is_empty());
    assert!(secrets.list(&labels).await.unwrap().items.is_empty());
    namespaces
        .delete(&namespace, &DeleteParams::background())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_batches_jobs_as_kubernetes_jobs() {
    if std::env::var("SOURCEGULL_TEST_KUBERNETES_POD_HOST").is_err() {
        eprintln!("skipping: SOURCEGULL_TEST_KUBERNETES_POD_HOST is not set");
        return;
    }
    let Some(server) = Server::start("127.0.0.1").await else {
        return;
    };
    let client = kube::Client::try_default().await.expect("a cluster");
    let namespace = format!(
        "sourcegull-test-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let namespaces: Api<Namespace> = Api::all(client.clone());
    namespaces
        .create(
            &PostParams::default(),
            &serde_json::from_value(serde_json::json!({
                "metadata": { "name": namespace }
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    // Steps run in their own pods and the executor checks out and talks to the server
    // itself, so pods need not reach the server.
    let runtime = Arc::new(KubernetesRuntime::with_client(
        client.clone(),
        KubernetesOptions {
            namespace: namespace.clone(),
            helper_image: HELPER.into(),
            memory_limit: Some("256Mi".into()),
            start_timeout: Duration::from_secs(180),
            ..Default::default()
        },
    ));
    let name =
        crate::common::batches::run_scenario(&server, runtime, Duration::from_secs(300)).await;

    // Nothing is left behind.
    let labels = ListParams::default().labels(&format!("io.sourcegull.executor={name}"));
    let kube_jobs: Api<KubeJob> = Api::namespaced(client.clone(), &namespace);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &namespace);
    assert!(kube_jobs.list(&labels).await.unwrap().items.is_empty());
    assert!(secrets.list(&labels).await.unwrap().items.is_empty());
    namespaces
        .delete(&namespace, &DeleteParams::background())
        .await
        .unwrap();
}
