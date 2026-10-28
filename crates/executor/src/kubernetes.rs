//! The Kubernetes runtime: one `Job` per workspace execution.
//!
//! The Job's pod shares an `emptyDir` workspace at `/data` between its containers. Init
//! containers run in order: the checkout (`setup.git`, the helper image with git fetching
//! from `/.executors/git` with the job token, which comes from a per-job Secret), then one
//! container per step. The main container holds the pod open once the steps are done, and the
//! executor copies the index out of it (`cat` over the exec API) and uploads it itself; for a
//! job without an upload it exits at once. Each container's output streams into its own log
//! entry while it runs.
//!
//! Executor secrets are keys of the per-job Secret (`env.NAME`) that step containers read
//! through `secretKeyRef`, so their values never appear in the pod spec. The job's
//! `DOCKER_AUTH_CONFIG` credentials become a second per-job Secret of type
//! `kubernetes.io/dockerconfigjson`, listed in the pod's `imagePullSecrets`.
//!
//! The executor deletes the Job and the Secrets when the job ends, is canceled or fails; the
//! Secrets are also owned by the Job, so Kubernetes collects them with the Job, and Jobs carry
//! a deadline and a TTL for executors that die mid-job.
//!
//! Jobs of the `batches` queue run with the batch step runner ([`crate::batches`]) on
//! [`steps::KubernetesSteps`]: one Kubernetes Job per step, the working copy copied in and out
//! (see [`steps`] for why).

pub mod steps;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{AsyncBufReadExt, TryStreamExt};
use k8s_openapi::api::batch::v1::{Job as KubeJob, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStatus, EmptyDirVolumeSource, EnvVar, EnvVarSource,
    LocalObjectReference, Pod, PodSpec, PodTemplateSpec, ResourceRequirements, Secret,
    SecretKeySelector, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;
use kube::api::{
    Api, AttachParams, DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams,
};
use sourcegull_batches_steps::docker::ImageUser;
use sourcegull_executor_api::{Job, capability, valid_tag_name};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::docker::{repo_path, step_script, step_workdir};
use crate::logger::StepLog;
use crate::{JobContext, JobError, Runtime};

/// Where the workspace volume is mounted in every container.
const DATA: &str = "/data";
/// Label with the executor's name on everything the executor creates.
const LABEL: &str = "io.sourcegull.executor";
/// Label with the Kubernetes Job's name on its pod, to find it.
const JOB_LABEL: &str = "io.sourcegull.executor/job";
/// Key of the git authorization header in the job's Secret.
const SECRET_KEY: &str = "git-auth-header";
/// Prefix of the executor secrets' keys in the job's Secret.
const SECRET_ENV_PREFIX: &str = "env.";
/// Suffix of the name of the job's registry credentials Secret.
const REGISTRY_SECRET_SUFFIX: &str = "-registry";
/// The main container's exit status when the index file is missing.
const NO_INDEX_STATUS: i32 = 3;
/// How often pod status is polled.
const POLL: Duration = Duration::from_millis(500);

/// The checkout script, run by `sh -c` with the sparse checkout directories as arguments.
/// Files are world-writable (`umask 0000`) so steps running as any user can write into the
/// workspace.
const CHECKOUT_SCRIPT: &str = r#"set -eu
umask 0000
mkdir -p /data/repository
cd /data/repository
echo "+ git init"
git init --quiet
git remote add origin "$SOURCEGULL_GIT_URL"
echo "+ git fetch $SOURCEGULL_FETCH_FLAGS origin $SOURCEGULL_COMMIT"
git fetch --progress --no-recurse-submodules $SOURCEGULL_FETCH_FLAGS origin "$SOURCEGULL_COMMIT" 2>&1 | tr '\r' '\n'
if [ "$#" -gt 0 ]; then
  echo "+ git sparse-checkout set --cone -- $*"
  git sparse-checkout set --cone -- "$@"
fi
echo "+ git checkout --detach $SOURCEGULL_COMMIT"
git checkout --quiet --detach "$SOURCEGULL_COMMIT"
printf '%s\n' "${SOURCEGULL_COMMIT_TAGS:-}" | while IFS= read -r tag; do
  [ -n "$tag" ] || continue
  printf '+ git tag %s %s\n' "$tag" "$SOURCEGULL_COMMIT"
  git tag "$tag" "$SOURCEGULL_COMMIT" || echo "creating tag $tag failed; continuing"
done
"#;

/// Settings of the Kubernetes runtime.
#[derive(Debug, Clone)]
pub struct KubernetesOptions {
    /// Namespace of the job pods (and their Secrets).
    pub namespace: String,
    /// Image of the checkout and main containers: needs `sh`, `git`, `cat` and `sleep` (the
    /// Sourcegull image has them).
    pub helper_image: String,
    /// The web role's URL as seen from job pods, when it differs from the executor's.
    pub pod_frontend_url: Option<url::Url>,
    /// `runtimeClassName` of job pods (e.g. gVisor or Kata for untrusted code).
    pub runtime_class: Option<String>,
    /// `nodeSelector` of job pods.
    pub node_selector: BTreeMap<String, String>,
    /// Tolerations of job pods.
    pub tolerations: Vec<Toleration>,
    /// Service account of job pods (its token is never mounted); the namespace default when
    /// unset.
    pub service_account: Option<String>,
    /// Image pull secrets of job pods.
    pub image_pull_secrets: Vec<String>,
    /// Resources of each step container, as Kubernetes quantities (`2`, `500m`, `4Gi`).
    pub cpu_request: Option<String>,
    pub memory_request: Option<String>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    /// Size limit of the workspace `emptyDir`.
    pub workspace_size: Option<String>,
    /// `activeDeadlineSeconds` of each Job.
    pub deadline: Duration,
    /// How long a pod may wait to be scheduled and to start a container (pulling images
    /// included) before the job is retried elsewhere.
    pub start_timeout: Duration,
    /// Keep Jobs after they end (for debugging); the TTL still removes them.
    pub keep_jobs: bool,
}

impl Default for KubernetesOptions {
    fn default() -> Self {
        Self {
            namespace: "default".into(),
            helper_image: "sourcegull/sourcegull:latest".into(),
            pod_frontend_url: None,
            runtime_class: None,
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            service_account: None,
            image_pull_secrets: Vec::new(),
            cpu_request: None,
            memory_request: None,
            cpu_limit: None,
            memory_limit: None,
            workspace_size: None,
            deadline: Duration::from_secs(4 * 3600),
            start_timeout: Duration::from_secs(10 * 60),
            keep_jobs: false,
        }
    }
}

/// Runs jobs as Kubernetes Jobs. Clones share the client and the image digest cache.
#[derive(Clone)]
pub struct KubernetesRuntime {
    client: kube::Client,
    options: KubernetesOptions,
    /// Image digests resolved for batch steps ([`steps`]), with the user the image runs as.
    #[allow(clippy::type_complexity)]
    digests: Arc<Mutex<HashMap<String, (String, Option<ImageUser>, Instant)>>>,
}

impl KubernetesRuntime {
    /// Connects with the in-cluster configuration, else `KUBECONFIG` / `~/.kube/config`. An
    /// empty namespace means the configuration's (the executor's own namespace in a cluster).
    pub async fn connect(mut options: KubernetesOptions) -> anyhow::Result<Self> {
        let client = kube::Client::try_default().await?;
        if options.namespace.is_empty() {
            options.namespace = client.default_namespace().to_owned();
        }
        Ok(Self::with_client(client, options))
    }

    /// The namespace of job pods.
    pub fn namespace(&self) -> &str {
        &self.options.namespace
    }

    /// Uses an existing client.
    pub fn with_client(client: kube::Client, options: KubernetesOptions) -> Self {
        Self {
            client,
            options,
            digests: Arc::default(),
        }
    }

    fn jobs(&self) -> Api<KubeJob> {
        Api::namespaced(self.client.clone(), &self.options.namespace)
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.options.namespace)
    }

    fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.client.clone(), &self.options.namespace)
    }
}

/// Parses tolerations given as a JSON array of Kubernetes tolerations.
pub fn parse_tolerations(json: &str) -> Result<Vec<Toleration>, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid tolerations: {e}"))
}

/// Parses `key=value` node selector entries.
pub fn parse_node_selector(entries: &[String]) -> Result<BTreeMap<String, String>, String> {
    entries
        .iter()
        .filter(|e| !e.trim().is_empty())
        .map(|e| match e.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_owned(), v.trim().to_owned())),
            _ => Err(format!(
                "invalid node selector entry {e:?} (want key=value)"
            )),
        })
        .collect()
}

fn errored(what: &str, e: impl std::fmt::Display) -> JobError {
    JobError::Errored(format!("{what}: {e}"))
}

/// What a container of the job pod is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Checkout,
    Step,
    /// Holds the pod open for the index copy.
    Upload,
    /// Ends the pod of a job without an upload.
    Done,
}

/// One container of the job pod, in the order they run.
#[derive(Debug, Clone)]
pub struct PlannedContainer {
    pub name: String,
    /// The log entry's key.
    pub key: String,
    /// The command shown in the log entry.
    pub command: Vec<String>,
    pub role: Role,
}

/// A label value: at most 63 characters of `[A-Za-z0-9_.-]`, starting and ending with an
/// alphanumeric character.
pub fn label_value(s: &str) -> String {
    let v: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    v.trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_owned()
}

/// The name of the Kubernetes Job (and Secret) for a job: a DNS label.
pub fn job_name(job: &Job, suffix: &str) -> String {
    let queue: String = job
        .queue
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(20)
        .collect();
    format!("sourcegull-{queue}-{}-{suffix}", job.id)
}

/// The executor secrets of `job` as `(name, value)`.
fn secret_env(job: &Job) -> Vec<(&str, &str)> {
    job.secret_env
        .iter()
        .map(|e| e.split_once('=').unwrap_or((e.as_str(), "")))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// What [`plan`] builds for one job.
pub struct Plan {
    /// The job's Secret (job token, executor secrets) first, then the registry credentials
    /// Secret if the job has any.
    pub secrets: Vec<Secret>,
    pub job: KubeJob,
    /// The containers in run order.
    pub containers: Vec<PlannedContainer>,
}

/// The labels of a Kubernetes Job named `name` (and of its pod and Secrets).
fn labels(executor: &str, name: &str) -> BTreeMap<String, String> {
    [
        (LABEL.to_owned(), label_value(executor)),
        (JOB_LABEL.to_owned(), name.to_owned()),
        (
            "app.kubernetes.io/managed-by".to_owned(),
            "sourcegull-executor".to_owned(),
        ),
    ]
    .into()
}

/// Builds the Secrets and the Job for `job`, and the containers in run order.
pub fn plan(
    job: &Job,
    options: &KubernetesOptions,
    name: &str,
    executor: &str,
    git_url: &str,
) -> Result<Plan, JobError> {
    let labels = labels(executor, name);
    let meta = ObjectMeta {
        name: Some(name.to_owned()),
        namespace: Some(options.namespace.clone()),
        labels: Some(labels),
        ..Default::default()
    };
    let mut string_data: BTreeMap<String, String> = [(
        SECRET_KEY.to_owned(),
        format!("Authorization: Bearer {}", job.token),
    )]
    .into();
    for (k, v) in secret_env(job) {
        string_data.insert(format!("{SECRET_ENV_PREFIX}{k}"), v.to_owned());
    }
    let mut secrets = vec![Secret {
        metadata: meta.clone(),
        string_data: Some(string_data),
        type_: Some("Opaque".into()),
        ..Default::default()
    }];
    let pull_secrets = registry_secret(job, options, &meta, &mut secrets)?;
    let mount = vec![VolumeMount {
        name: "workspace".into(),
        mount_path: DATA.into(),
        ..Default::default()
    }];
    let env = |name: &str, value: &str| EnvVar {
        name: name.to_owned(),
        value: Some(value.to_owned()),
        ..Default::default()
    };

    let mut planned = Vec::new();
    let mut init = Vec::new();

    let mut fetch_flags = Vec::new();
    if job.shallow_clone {
        fetch_flags.push("--depth=1");
    }
    fetch_flags.push(if job.fetch_tags {
        "--tags"
    } else {
        "--no-tags"
    });
    let fetch_flags = fetch_flags.join(" ");
    // One name per line; valid names contain no control characters.
    let commit_tags = job
        .commit_tags
        .iter()
        .filter(|t| valid_tag_name(t))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let mut checkout_args = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        CHECKOUT_SCRIPT.to_owned(),
        "checkout".to_owned(),
    ];
    checkout_args.extend(job.sparse_checkout.iter().cloned());
    init.push(Container {
        name: "setup-git".into(),
        image: Some(options.helper_image.clone()),
        command: Some(checkout_args),
        working_dir: Some(DATA.into()),
        env: Some(vec![
            env("SOURCEGULL_GIT_URL", git_url),
            env("SOURCEGULL_COMMIT", &job.commit),
            env("SOURCEGULL_FETCH_FLAGS", &fetch_flags),
            env("SOURCEGULL_COMMIT_TAGS", &commit_tags),
            env("HOME", "/tmp"),
            env("GIT_TERMINAL_PROMPT", "0"),
            env("GIT_CONFIG_NOSYSTEM", "1"),
            env("GIT_CONFIG_COUNT", "1"),
            env("GIT_CONFIG_KEY_0", "http.extraHeader"),
            EnvVar {
                name: "GIT_CONFIG_VALUE_0".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: name.to_owned(),
                        key: SECRET_KEY.into(),
                        optional: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        volume_mounts: Some(mount.clone()),
        ..Default::default()
    });
    let mut git_command = vec![
        "git".to_owned(),
        "fetch".to_owned(),
        "origin".to_owned(),
        job.commit.clone(),
    ];
    if !job.sparse_checkout.is_empty() {
        git_command.push("(sparse)".into());
    }
    planned.push(PlannedContainer {
        name: "setup-git".into(),
        key: "setup.git".into(),
        command: git_command,
        role: Role::Checkout,
    });

    let resources = step_resources(options);
    let name_of_job = name;
    for (i, step) in job.docker_steps.iter().enumerate() {
        let workdir = step_workdir(step)?;
        let name = format!("step-{i}");
        let mut step_env = Vec::with_capacity(step.env.len() + job.secret_env.len());
        for entry in &step.env {
            let (k, v) = entry.split_once('=').unwrap_or((entry, ""));
            step_env.push(env(k, v));
        }
        for (k, _) in secret_env(job) {
            step_env.push(EnvVar {
                name: k.to_owned(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: name_of_job.to_owned(),
                        key: format!("{SECRET_ENV_PREFIX}{k}"),
                        optional: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        init.push(Container {
            name: name.clone(),
            image: Some(step.image.clone()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), step_script(step)]),
            working_dir: Some(workdir.clone()),
            env: Some(step_env),
            resources: resources.clone(),
            volume_mounts: Some(mount.clone()),
            ..Default::default()
        });
        let mut command = vec![
            "kubernetes".to_owned(),
            "--workdir".to_owned(),
            workdir,
            step.image.clone(),
        ];
        command.extend(step.commands.iter().cloned());
        planned.push(PlannedContainer {
            name,
            key: step.key.clone(),
            command,
            role: Role::Step,
        });
    }

    let main = match &job.upload {
        Some(upload) => {
            let path = repo_path(&upload.file)?;
            planned.push(PlannedContainer {
                name: "upload".into(),
                key: upload.key.clone(),
                command: vec!["upload".to_owned(), upload.file.clone()],
                role: Role::Upload,
            });
            Container {
                name: "upload".into(),
                image: Some(options.helper_image.clone()),
                // Wait for the executor to copy the index out; it deletes the Job then.
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "[ -f \"$0\" ] || exit {NO_INDEX_STATUS}; exec sleep {}",
                        options.deadline.as_secs().max(60)
                    ),
                    path,
                ]),
                volume_mounts: Some(mount.clone()),
                ..Default::default()
            }
        }
        None => {
            planned.push(PlannedContainer {
                name: "done".into(),
                key: String::new(),
                command: Vec::new(),
                role: Role::Done,
            });
            Container {
                name: "done".into(),
                image: Some(options.helper_image.clone()),
                command: Some(vec!["/bin/sh".into(), "-c".into(), "true".into()]),
                ..Default::default()
            }
        }
    };

    let pod = PodSpec {
        init_containers: Some(init),
        containers: vec![main],
        volumes: Some(vec![workspace_volume("workspace", options)]),
        ..Default::default()
    };
    Ok(Plan {
        secrets,
        job: pod_job(options, meta, pod, pull_secrets),
        containers: planned,
    })
}

/// An `emptyDir` volume with the workspace size limit.
fn workspace_volume(name: &str, options: &KubernetesOptions) -> Volume {
    Volume {
        name: name.into(),
        empty_dir: Some(EmptyDirVolumeSource {
            size_limit: options.workspace_size.clone().map(Quantity),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The Kubernetes Job running `pod` once: the pod settings every job pod shares (runtime
/// class, scheduling, service account without a token, pull secrets), no retries, the
/// deadline, and a TTL. The pod gets the labels of `meta`.
fn pod_job(
    options: &KubernetesOptions,
    meta: ObjectMeta,
    pod: PodSpec,
    pull_secrets: Vec<LocalObjectReference>,
) -> KubeJob {
    let pod = PodSpec {
        restart_policy: Some("Never".into()),
        runtime_class_name: options.runtime_class.clone(),
        node_selector: (!options.node_selector.is_empty()).then(|| options.node_selector.clone()),
        tolerations: (!options.tolerations.is_empty()).then(|| options.tolerations.clone()),
        service_account_name: options.service_account.clone(),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        image_pull_secrets: (!pull_secrets.is_empty()).then_some(pull_secrets),
        ..pod
    };
    KubeJob {
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(options.deadline.as_secs().max(60) as i64),
            ttl_seconds_after_finished: Some(600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: meta.labels.clone(),
                    ..Default::default()
                }),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        metadata: meta,
        ..Default::default()
    }
}

/// Adds the Secret with the job's `DOCKER_AUTH_CONFIG` credentials (named after `meta` with
/// [`REGISTRY_SECRET_SUFFIX`]) to `secrets` if the job has any, and returns the pod's image
/// pull secrets: that one first, then the configured ones.
fn registry_secret(
    job: &Job,
    options: &KubernetesOptions,
    meta: &ObjectMeta,
    secrets: &mut Vec<Secret>,
) -> Result<Vec<LocalObjectReference>, JobError> {
    let mut pull_secrets: Vec<LocalObjectReference> = options
        .image_pull_secrets
        .iter()
        .map(|n| LocalObjectReference { name: n.clone() })
        .collect();
    if let Some(config) = &job.docker_auth_config {
        let registry = format!(
            "{}{REGISTRY_SECRET_SUFFIX}",
            meta.name.as_deref().unwrap_or_default()
        );
        let json = serde_json::to_string(config)
            .map_err(|e| errored("encoding the registry credentials", e))?;
        secrets.push(Secret {
            metadata: ObjectMeta {
                name: Some(registry.clone()),
                ..meta.clone()
            },
            string_data: Some([(".dockerconfigjson".to_owned(), json)].into()),
            type_: Some("kubernetes.io/dockerconfigjson".into()),
            ..Default::default()
        });
        pull_secrets.insert(0, LocalObjectReference { name: registry });
    }
    Ok(pull_secrets)
}

fn step_resources(options: &KubernetesOptions) -> Option<ResourceRequirements> {
    let pair = |cpu: &Option<String>, memory: &Option<String>| {
        let mut m = BTreeMap::new();
        if let Some(c) = cpu {
            m.insert("cpu".to_owned(), Quantity(c.clone()));
        }
        if let Some(mem) = memory {
            m.insert("memory".to_owned(), Quantity(mem.clone()));
        }
        (!m.is_empty()).then_some(m)
    };
    let requests = pair(&options.cpu_request, &options.memory_request);
    let limits = pair(&options.cpu_limit, &options.memory_limit);
    (requests.is_some() || limits.is_some()).then(|| ResourceRequirements {
        requests,
        limits,
        ..Default::default()
    })
}

/// A container's progress, as seen in the pod status.
#[derive(Debug, PartialEq, Eq)]
enum Progress {
    /// Not started yet (scheduling, pulling, earlier containers running).
    Waiting,
    Running,
    Terminated(i32),
}

/// Everything created for one job, removed by [`KubernetesRuntime::cleanup`].
#[derive(Default)]
struct Resources {
    job: Option<String>,
    secrets: Vec<String>,
}

#[async_trait]
impl Runtime for KubernetesRuntime {
    fn name(&self) -> &'static str {
        "kubernetes"
    }

    async fn version(&self) -> String {
        self.client
            .apiserver_version()
            .await
            .map(|v| v.git_version)
            .unwrap_or_default()
    }

    fn capabilities(&self) -> &'static [&'static str] {
        &[
            capability::DOCKER_STEPS,
            capability::SCIP_UPLOAD,
            capability::SECRETS,
            capability::BATCH_STEPS,
        ]
    }

    async fn run(&self, job: &Job, ctx: &JobContext) -> Result<(), JobError> {
        if job.batches.is_some() {
            return self.run_batches(job, ctx).await;
        }
        let mut res = Resources::default();
        let result = self.run_inner(job, ctx, &mut res).await;
        self.cleanup(res).await;
        result
    }
}

impl KubernetesRuntime {
    async fn run_inner(
        &self,
        job: &Job,
        ctx: &JobContext,
        res: &mut Resources,
    ) -> Result<(), JobError> {
        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        let name = job_name(job, &suffix);
        let git_url = match &self.options.pod_frontend_url {
            Some(base) => {
                let mut u = base.clone();
                u.set_path(&format!("/.executors/git/{}", job.repository_name));
                u.to_string()
            }
            None => ctx.client.git_url(&job.repository_name).to_string(),
        };
        let Plan {
            secrets,
            job: kube_job,
            containers: planned,
        } = plan(job, &self.options, &name, &ctx.executor, &git_url)?;

        self.create_job(&secrets, &kube_job, res).await?;

        let pod = self.find_pod(&name, &ctx.cancel).await?;
        for container in &planned {
            match container.role {
                Role::Checkout | Role::Step => {
                    let log = ctx.logger.start(&container.key, &container.command).await?;
                    match self.run_container(&pod, container, &log, &ctx.cancel).await {
                        Ok(0) => log.finish(Some(0)).await?,
                        Ok(code) => {
                            log.finish(Some(code)).await?;
                            return Err(if container.role == Role::Checkout {
                                JobError::Errored(format!("the checkout exited with status {code}"))
                            } else {
                                JobError::Failed(format!(
                                    "step {} exited with status {code}",
                                    container.key
                                ))
                            });
                        }
                        Err(e) => {
                            if !matches!(e, JobError::Canceled) {
                                log.line(&e.to_string());
                            }
                            log.finish(None).await?;
                            return Err(e);
                        }
                    }
                }
                Role::Upload => {
                    let log = ctx.logger.start(&container.key, &container.command).await?;
                    let result = self.upload(job, ctx, &pod, container, &log).await;
                    match &result {
                        Ok(()) => log.finish(Some(0)).await?,
                        Err(JobError::Canceled) => log.finish(None).await?,
                        Err(e) => {
                            log.line(&e.to_string());
                            log.finish(Some(1)).await?;
                        }
                    }
                    result?;
                }
                Role::Done => {}
            }
        }
        Ok(())
    }

    /// Creates `secrets` and then `kube_job`, recording both in `res`, and makes the Job the
    /// Secrets' owner so Kubernetes collects them with it if the executor dies.
    async fn create_job(
        &self,
        secrets: &[Secret],
        kube_job: &KubeJob,
        res: &mut Resources,
    ) -> Result<(), JobError> {
        for secret in secrets {
            self.secrets()
                .create(&PostParams::default(), secret)
                .await
                .map_err(|e| errored("creating the job's secret", e))?;
            res.secrets.extend(secret.metadata.name.clone());
        }
        let name = kube_job.metadata.name.clone().unwrap_or_default();
        let created = self
            .jobs()
            .create(&PostParams::default(), kube_job)
            .await
            .map_err(|e| match &e {
                kube::Error::Api(s) if s.code == 422 || s.code == 400 => {
                    JobError::Failed(format!("the job's pod is invalid: {}", s.message))
                }
                _ => errored("creating the Kubernetes Job", e),
            })?;
        res.job = Some(name.clone());
        if let Some(uid) = created.meta().uid.clone() {
            let owner = OwnerReference {
                api_version: KubeJob::api_version(&()).into_owned(),
                kind: KubeJob::kind(&()).into_owned(),
                name,
                uid,
                ..Default::default()
            };
            let patch = serde_json::json!({ "metadata": { "ownerReferences": [owner] } });
            for secret in &res.secrets {
                if let Err(e) = self
                    .secrets()
                    .patch(secret, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                {
                    tracing::warn!(error = %e, secret, "setting the secret's owner failed");
                }
            }
        }
        Ok(())
    }

    /// Waits for the Job's pod to appear and returns its name.
    async fn find_pod(
        &self,
        job_name: &str,
        cancel: &CancellationToken,
    ) -> Result<String, JobError> {
        let params = ListParams::default().labels(&format!("{JOB_LABEL}={job_name}"));
        let deadline = Instant::now() + self.options.start_timeout;
        loop {
            let pods = self
                .pods()
                .list(&params)
                .await
                .map_err(|e| errored("listing the job's pods", e))?;
            if let Some(name) = pods.items.into_iter().find_map(|p| p.metadata.name) {
                return Ok(name);
            }
            if Instant::now() > deadline {
                return Err(JobError::Errored(
                    "the Kubernetes Job created no pod in time".into(),
                ));
            }
            self.sleep(cancel).await?;
        }
    }

    async fn sleep(&self, cancel: &CancellationToken) -> Result<(), JobError> {
        tokio::select! {
            _ = tokio::time::sleep(POLL) => Ok(()),
            _ = cancel.cancelled() => Err(JobError::Canceled),
        }
    }

    /// Polls the pod until `container` runs or has ended. Fails on image pull errors and when
    /// the pod does not get there within the start timeout.
    async fn wait_started(
        &self,
        pod: &str,
        container: &str,
        cancel: &CancellationToken,
    ) -> Result<Progress, JobError> {
        let deadline = Instant::now() + self.options.start_timeout;
        loop {
            let p = self
                .pods()
                .get(pod)
                .await
                .map_err(|e| errored("reading the job's pod", e))?;
            let status = p.status.unwrap_or_default();
            let statuses = status
                .init_container_statuses
                .iter()
                .flatten()
                .chain(status.container_statuses.iter().flatten());
            let mut waiting_reason = None;
            if let Some(cs) = statuses.into_iter().find(|s| s.name == container) {
                match progress(cs) {
                    Progress::Waiting => {
                        if let Some(w) = cs.state.as_ref().and_then(|s| s.waiting.as_ref()) {
                            let reason = w.reason.clone().unwrap_or_default();
                            let message = w.message.clone().unwrap_or_default();
                            // ErrImagePull alone may be transient; the back-off follows a retry.
                            if matches!(
                                reason.as_str(),
                                "ImagePullBackOff" | "InvalidImageName" | "ErrImageNeverPull"
                            ) {
                                return Err(JobError::Failed(format!(
                                    "pulling {}: {message}",
                                    cs.image
                                )));
                            }
                            if !reason.is_empty() && reason != "PodInitializing" {
                                waiting_reason = Some(format!("{reason}: {message}"));
                            }
                        }
                    }
                    p => return Ok(p),
                }
            }
            match status.phase.as_deref() {
                Some("Failed") => {
                    let why = status
                        .message
                        .or(status.reason)
                        .unwrap_or_else(|| "the pod failed".into());
                    return Err(JobError::Errored(why));
                }
                Some("Succeeded") => {
                    return Err(JobError::Errored(format!(
                        "the pod ended before {container} ran"
                    )));
                }
                _ => {}
            }
            if Instant::now() > deadline {
                let why = waiting_reason
                    .or_else(|| {
                        status.conditions.iter().flatten().find_map(|c| {
                            (c.type_ == "PodScheduled" && c.status != "True")
                                .then(|| c.message.clone().unwrap_or_default())
                        })
                    })
                    .unwrap_or_default();
                return Err(JobError::Errored(format!(
                    "{container} did not start in time {why}"
                )));
            }
            self.sleep(cancel).await?;
        }
    }

    /// Streams a container's output into `log` and returns its exit code.
    async fn run_container(
        &self,
        pod: &str,
        container: &PlannedContainer,
        log: &StepLog,
        cancel: &CancellationToken,
    ) -> Result<i32, JobError> {
        if let Progress::Terminated(code) = self.wait_started(pod, &container.name, cancel).await? {
            // Ended between polls: the logs are complete.
            self.copy_logs(pod, &container.name, log, false, false, cancel)
                .await?;
            return Ok(code);
        }
        self.copy_logs(pod, &container.name, log, true, false, cancel)
            .await?;
        // The log stream ends when the container does, and its status follows shortly after.
        // A stream the API server dropped while the container still runs is reopened at its end.
        let mut running_polls = 0;
        loop {
            match self.wait_started(pod, &container.name, cancel).await? {
                Progress::Terminated(code) => return Ok(code),
                Progress::Running if running_polls >= 4 => {
                    running_polls = 0;
                    self.copy_logs(pod, &container.name, log, true, true, cancel)
                        .await?;
                }
                _ => running_polls += 1,
            }
            self.sleep(cancel).await?;
        }
    }

    async fn copy_logs(
        &self,
        pod: &str,
        container: &str,
        log: &StepLog,
        follow: bool,
        from_now: bool,
        cancel: &CancellationToken,
    ) -> Result<(), JobError> {
        let params = LogParams {
            container: Some(container.to_owned()),
            follow,
            tail_lines: from_now.then_some(0),
            ..Default::default()
        };
        let stream = self
            .pods()
            .log_stream(pod, &params)
            .await
            .map_err(|e| errored("reading the container's output", e))?;
        let mut lines = stream.lines();
        loop {
            tokio::select! {
                line = lines.try_next() => match line {
                    Ok(Some(l)) => log.line(&l),
                    Ok(None) => return Ok(()),
                    Err(e) => {
                        tracing::debug!(error = %e, container, "reading container logs failed");
                        return Ok(());
                    }
                },
                _ = cancel.cancelled() => {
                    log.line("canceled");
                    return Err(JobError::Canceled);
                }
            }
        }
    }

    /// Copies the index out of the main container and uploads it.
    async fn upload(
        &self,
        job: &Job,
        ctx: &JobContext,
        pod: &str,
        container: &PlannedContainer,
        log: &StepLog,
    ) -> Result<(), JobError> {
        let Some(upload) = &job.upload else {
            return Ok(());
        };
        match self.wait_started(pod, &container.name, &ctx.cancel).await? {
            Progress::Terminated(NO_INDEX_STATUS) => {
                return Err(JobError::Failed(format!(
                    "the indexer wrote no {}",
                    upload.file
                )));
            }
            Progress::Terminated(code) => {
                return Err(JobError::Errored(format!(
                    "the upload container exited with status {code}"
                )));
            }
            _ => {}
        }
        let path = repo_path(&upload.file)?;
        let local = ctx.scratch.join("index.upload");
        let cat = ["cat", path.as_str()];
        tokio::select! {
            r = self.exec_to_file(pod, &container.name, &cat, &local, "copying the index") => r?,
            _ = ctx.cancel.cancelled() => return Err(JobError::Canceled),
        }
        let size = tokio::fs::metadata(&local)
            .await
            .map_err(|e| errored("copying the index", e))?
            .len();
        log.line(&format!("uploading {} ({size} bytes)", upload.file));
        let id = ctx
            .client
            .upload_scip(job, &upload.root, &local)
            .await
            .map_err(|e| errored("uploading the index", e))?;
        log.line(&format!("upload {id} queued for processing"));
        Ok(())
    }

    /// Runs `command` in a running container over the exec API and writes its output to
    /// `dest`; `what` prefixes errors.
    async fn exec_to_file(
        &self,
        pod: &str,
        container: &str,
        command: &[&str],
        dest: &Path,
        what: &str,
    ) -> Result<(), JobError> {
        let mut attached = self
            .pods()
            .exec(
                pod,
                command.to_vec(),
                &AttachParams::default()
                    .container(container)
                    .stdin(false)
                    .stdout(true)
                    .stderr(true),
            )
            .await
            .map_err(|e| errored(what, e))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| JobError::Errored(format!("{what}: no output stream")))?;
        let mut stderr = attached.stderr();
        let status = attached.take_status();
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| errored(what, e))?;
        let copy = tokio::io::copy(&mut stdout, &mut file);
        let errors = async {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(s, &mut buf).await;
            }
            buf
        };
        let (copied, err_out) = tokio::join!(copy, errors);
        copied.map_err(|e| errored(what, e))?;
        file.flush().await.map_err(|e| errored(what, e))?;
        let status = match status {
            Some(s) => s.await,
            None => None,
        };
        attached.join().await.map_err(|e| errored(what, e))?;
        match status {
            Some(s) if s.status.as_deref() == Some("Success") => Ok(()),
            Some(s) => Err(JobError::Errored(format!(
                "{what}: {} {}",
                s.message.unwrap_or_default(),
                String::from_utf8_lossy(&err_out).trim()
            ))),
            None => Err(JobError::Errored(format!("{what}: no exit status"))),
        }
    }

    async fn cleanup(&self, res: Resources) {
        if self.options.keep_jobs {
            return;
        }
        if let Some(job) = &res.job
            && let Err(e) = self.jobs().delete(job, &DeleteParams::background()).await
        {
            tracing::warn!(error = %e, job, "deleting a Kubernetes Job failed");
        }
        for secret in &res.secrets {
            if let Err(e) = self
                .secrets()
                .delete(secret, &DeleteParams::default())
                .await
                && !matches!(&e, kube::Error::Api(s) if s.code == 404)
            {
                tracing::warn!(error = %e, secret, "deleting a job secret failed");
            }
        }
    }
}

fn progress(cs: &ContainerStatus) -> Progress {
    match &cs.state {
        Some(ContainerState {
            terminated: Some(t),
            ..
        }) => Progress::Terminated(t.exit_code),
        Some(ContainerState {
            running: Some(_), ..
        }) => Progress::Running,
        _ => Progress::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_executor_api::{DockerStep, UploadStep};

    fn job() -> Job {
        Job {
            id: 42,
            queue: "codeintel".into(),
            token: "sjt_secret".into(),
            repository_name: "gitlab.example.com/acme/demo".into(),
            commit: "a".repeat(40),
            shallow_clone: true,
            sparse_checkout: vec!["web".into()],
            commit_tags: vec!["v1.2.0".into(), "-bad".into(), "web/v1.2.0".into()],
            docker_steps: vec![DockerStep {
                key: "step.docker.indexer".into(),
                image: "sourcegraph/scip-go".into(),
                commands: vec!["scip-go".into()],
                dir: "web".into(),
                env: vec!["GOFLAGS=-mod=mod".into(), "EMPTY".into()],
            }],
            upload: Some(UploadStep {
                key: "step.upload".into(),
                file: "web/index.scip".into(),
                root: "web/".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn parses_options() {
        let t = parse_tolerations(
            r#"[{"key":"dedicated","operator":"Equal","value":"executors","effect":"NoSchedule"}]"#,
        )
        .unwrap();
        assert_eq!(t[0].key.as_deref(), Some("dedicated"));
        assert!(parse_tolerations("{").is_err());
        let sel = parse_node_selector(&["pool=executors".into(), " zone = a ".into()]).unwrap();
        assert_eq!(sel["pool"], "executors");
        assert_eq!(sel["zone"], "a");
        assert!(parse_node_selector(&["nokey".into()]).is_err());
    }

    #[test]
    fn names_and_labels() {
        assert_eq!(
            job_name(&job(), "ab12cd34"),
            "sourcegull-codeintel-42-ab12cd34"
        );
        assert_eq!(label_value("exec_1.host"), "exec_1.host");
        assert_eq!(label_value("-a b/c-"), "a-b-c");
        assert_eq!(label_value(&"x".repeat(80)).len(), 63);
    }

    #[test]
    fn plans_a_job_pod() {
        let options = KubernetesOptions {
            namespace: "jobs".into(),
            helper_image: "sourcegull/sourcegull:1".into(),
            runtime_class: Some("gvisor".into()),
            node_selector: [("pool".to_owned(), "executors".to_owned())].into(),
            memory_limit: Some("4Gi".into()),
            cpu_request: Some("500m".into()),
            ..Default::default()
        };
        let Plan {
            secrets,
            job: kjob,
            containers: planned,
        } = plan(
            &job(),
            &options,
            "sourcegull-codeintel-42-x",
            "exec 1",
            "http://web/.executors/git/r",
        )
        .unwrap();
        // The token only lives in the Secret.
        assert_eq!(secrets.len(), 1);
        assert_eq!(
            secrets[0].string_data.as_ref().unwrap()[SECRET_KEY],
            "Authorization: Bearer sjt_secret"
        );
        let json = serde_json::to_string(&kjob).unwrap();
        assert!(!json.contains("sjt_secret"));

        let keys: Vec<_> = planned.iter().map(|p| (p.key.as_str(), &p.role)).collect();
        assert_eq!(
            keys,
            [
                ("setup.git", &Role::Checkout),
                ("step.docker.indexer", &Role::Step),
                ("step.upload", &Role::Upload)
            ]
        );
        let spec = kjob.spec.unwrap();
        assert_eq!(spec.backoff_limit, Some(0));
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("gvisor"));
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.node_selector.unwrap()["pool"], "executors");
        let labels = spec.template.metadata.unwrap().labels.unwrap();
        assert_eq!(labels[JOB_LABEL], "sourcegull-codeintel-42-x");
        assert_eq!(labels[LABEL], "exec-1");

        let init = pod.init_containers.unwrap();
        assert_eq!(init.len(), 2);
        let checkout = &init[0];
        assert_eq!(checkout.image.as_deref(), Some("sourcegull/sourcegull:1"));
        let args = checkout.command.as_ref().unwrap();
        assert_eq!(&args[3..], ["checkout", "web"]);
        let env = checkout.env.as_ref().unwrap();
        let get = |n: &str| env.iter().find(|e| e.name == n).unwrap();
        assert_eq!(
            get("SOURCEGULL_FETCH_FLAGS").value.as_deref(),
            Some("--depth=1 --no-tags")
        );
        // Tag names go through the environment, never into the script; invalid ones are
        // dropped.
        assert_eq!(
            get("SOURCEGULL_COMMIT_TAGS").value.as_deref(),
            Some("v1.2.0\nweb/v1.2.0")
        );
        assert!(!args[2].contains("v1.2.0"));
        assert!(get("GIT_CONFIG_VALUE_0").value_from.is_some());

        let step = &init[1];
        assert_eq!(step.working_dir.as_deref(), Some("/data/repository/web"));
        assert_eq!(step.command.as_ref().unwrap()[2], "set -e\nscip-go\n");
        let step_env = step.env.as_ref().unwrap();
        assert_eq!(step_env[0].name, "GOFLAGS");
        assert_eq!(step_env[0].value.as_deref(), Some("-mod=mod"));
        assert_eq!(step_env[1].value.as_deref(), Some(""));
        let resources = step.resources.as_ref().unwrap();
        assert_eq!(resources.limits.as_ref().unwrap()["memory"].0, "4Gi");
        assert_eq!(resources.requests.as_ref().unwrap()["cpu"].0, "500m");

        let main = &pod.containers[0];
        assert_eq!(main.name, "upload");
        assert_eq!(
            main.command.as_ref().unwrap().last().unwrap(),
            "/data/repository/web/index.scip"
        );
    }

    #[test]
    fn checkout_script_creates_the_commit_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", tmp.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q"]);
        git(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "c1",
            ],
        );
        let commit = git(&src, &["rev-parse", "HEAD"]).trim().to_owned();
        let work = tmp.path().join("work");
        let script = CHECKOUT_SCRIPT.replace("/data/repository", work.to_str().unwrap());
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &script, "checkout"])
            .current_dir(tmp.path())
            .env("HOME", tmp.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("SOURCEGULL_GIT_URL", format!("file://{}", src.display()))
            .env("SOURCEGULL_COMMIT", &commit)
            .env("SOURCEGULL_FETCH_FLAGS", "--depth=1 --no-tags")
            .env("SOURCEGULL_COMMIT_TAGS", "v1.0\npkg/v2.0.0\n$(touch pwned)")
            .output()
            .unwrap();
        // The last name is not a valid tag (the runtime never sends it); creating it fails
        // and the checkout still succeeds.
        assert!(out.status.success(), "{out:?}");
        let tags = git(&work, &["tag", "--points-at", "HEAD"]);
        assert_eq!(tags.lines().collect::<Vec<_>>(), ["pkg/v2.0.0", "v1.0"]);
        assert!(!tmp.path().join("pwned").exists());
        assert!(!work.join("pwned").exists());
    }

    #[test]
    fn rejects_escaping_paths() {
        let mut j = job();
        j.docker_steps[0].dir = "../x".into();
        assert!(plan(&j, &KubernetesOptions::default(), "n", "e", "u").is_err());
        let mut j = job();
        j.upload.as_mut().unwrap().file = "../index.scip".into();
        assert!(plan(&j, &KubernetesOptions::default(), "n", "e", "u").is_err());
    }

    #[test]
    fn a_job_without_upload_ends_by_itself() {
        let mut j = job();
        j.upload = None;
        let p = plan(&j, &KubernetesOptions::default(), "n", "e", "u").unwrap();
        assert_eq!(p.containers.last().unwrap().role, Role::Done);
        let pod = p.job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.containers[0].name, "done");
    }

    #[test]
    fn secrets_stay_in_secrets() {
        let mut j = job();
        j.secret_env = vec!["API_KEY=hunter2-value".into()];
        j.docker_auth_config = Some(sourcegull_executor_api::DockerAuthConfig {
            auths: [(
                "registry.example.com".to_owned(),
                sourcegull_executor_api::DockerAuthConfigAuth {
                    auth: "dXNlcjpwYXNz".into(),
                },
            )]
            .into(),
        });
        let options = KubernetesOptions {
            image_pull_secrets: vec!["shared".into()],
            ..Default::default()
        };
        let p = plan(&j, &options, "sourcegull-codeintel-42-x", "e", "u").unwrap();
        assert_eq!(p.secrets.len(), 2);
        assert_eq!(
            p.secrets[0].string_data.as_ref().unwrap()["env.API_KEY"],
            "hunter2-value"
        );
        let registry = &p.secrets[1];
        assert_eq!(
            registry.metadata.name.as_deref(),
            Some("sourcegull-codeintel-42-x-registry")
        );
        assert_eq!(
            registry.type_.as_deref(),
            Some("kubernetes.io/dockerconfigjson")
        );
        let config: serde_json::Value =
            serde_json::from_str(&registry.string_data.as_ref().unwrap()[".dockerconfigjson"])
                .unwrap();
        assert_eq!(
            config["auths"]["registry.example.com"]["auth"],
            "dXNlcjpwYXNz"
        );

        let json = serde_json::to_string(&p.job).unwrap();
        assert!(!json.contains("hunter2-value"));
        assert!(!json.contains("dXNlcjpwYXNz"));
        let pod = p.job.spec.unwrap().template.spec.unwrap();
        let pulls: Vec<_> = pod
            .image_pull_secrets
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(pulls, ["sourcegull-codeintel-42-x-registry", "shared"]);
        let step = &pod.init_containers.unwrap()[1];
        let var = step
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "API_KEY")
            .unwrap();
        assert!(var.value.is_none());
        let selector = var
            .value_from
            .as_ref()
            .unwrap()
            .secret_key_ref
            .as_ref()
            .unwrap();
        assert_eq!(
            (selector.name.as_str(), selector.key.as_str()),
            ("sourcegull-codeintel-42-x", "env.API_KEY")
        );
    }
}
