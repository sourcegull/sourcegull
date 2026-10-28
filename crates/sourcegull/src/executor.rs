//! `sourcegull executor`: runs jobs from the web role's executor queues with the Docker or the
//! Kubernetes runtime.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use sourcegull_executor::docker::{DockerOptions, DockerRuntime};
use sourcegull_executor::kubernetes::{
    KubernetesOptions, KubernetesRuntime, parse_node_selector, parse_tolerations,
};
use sourcegull_executor::{ExecutorConfig, Runtime};
use tokio_util::sync::CancellationToken;

use crate::cli::{ExecutorArgs, ExecutorRuntime, KubernetesArgs, parse_memory};

/// The executor's name when none is given: the host name.
fn default_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "sourcegull-executor".to_owned())
}

/// Runs the executor until `cancel` fires.
pub async fn run(args: ExecutorArgs, cancel: CancellationToken) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.poll_interval > 0.0 && args.poll_interval.is_finite(),
        "--poll-interval must be positive"
    );
    let runtime: Arc<dyn Runtime> = match args.runtime {
        ExecutorRuntime::Docker => Arc::new(docker_runtime(&args).await?),
        ExecutorRuntime::Kubernetes => Arc::new(kubernetes_runtime(&args.kubernetes).await?),
    };
    let config = ExecutorConfig {
        url: args.url,
        token: args.token,
        name: args.name.unwrap_or_else(default_name),
        queues: args.queues,
        max_jobs: args.max_jobs,
        poll_interval: Duration::from_secs_f64(args.poll_interval),
        heartbeat_interval: Duration::from_secs(5),
        scratch_dir: args.scratch_dir,
        git: args.git,
    };
    sourcegull_executor::run(config, runtime, cancel).await
}

async fn docker_runtime(args: &ExecutorArgs) -> anyhow::Result<DockerRuntime> {
    let memory_bytes = match &args.docker_memory {
        Some(m) => parse_memory(m).map_err(anyhow::Error::msg)?,
        None => 0,
    };
    let runtime = DockerRuntime::connect(DockerOptions {
        network: args.docker_network.clone(),
        memory_bytes,
        cpus: args.docker_cpus.unwrap_or(0.0),
        always_pull: args.docker_always_pull,
    })
    .context("connecting to Docker")?;
    anyhow::ensure!(
        !runtime.version().await.is_empty(),
        "cannot reach the Docker daemon (DOCKER_HOST or /var/run/docker.sock)"
    );
    Ok(runtime)
}

async fn kubernetes_runtime(args: &KubernetesArgs) -> anyhow::Result<KubernetesRuntime> {
    let tolerations = match &args.tolerations {
        Some(json) if !json.trim().is_empty() => {
            parse_tolerations(json).map_err(anyhow::Error::msg)?
        }
        _ => Vec::new(),
    };
    let runtime = KubernetesRuntime::connect(KubernetesOptions {
        namespace: args.namespace.clone().unwrap_or_default(),
        helper_image: args.helper_image.clone(),
        pod_frontend_url: args.pod_frontend_url.clone(),
        runtime_class: args.runtime_class.clone().filter(|s| !s.is_empty()),
        node_selector: parse_node_selector(&args.node_selector).map_err(anyhow::Error::msg)?,
        tolerations,
        service_account: args.service_account.clone().filter(|s| !s.is_empty()),
        image_pull_secrets: args
            .image_pull_secrets
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect(),
        cpu_request: args.cpu_request.clone().filter(|s| !s.is_empty()),
        memory_request: args.memory_request.clone().filter(|s| !s.is_empty()),
        cpu_limit: args.cpu_limit.clone().filter(|s| !s.is_empty()),
        memory_limit: args.memory_limit.clone().filter(|s| !s.is_empty()),
        workspace_size: args.workspace_size.clone().filter(|s| !s.is_empty()),
        deadline: Duration::from_secs(args.deadline),
        start_timeout: Duration::from_secs(args.start_timeout),
        keep_jobs: args.keep_jobs,
    })
    .await
    .context("connecting to Kubernetes")?;
    anyhow::ensure!(
        !runtime.version().await.is_empty(),
        "cannot reach the Kubernetes API server in namespace {}",
        runtime.namespace()
    );
    Ok(runtime)
}
