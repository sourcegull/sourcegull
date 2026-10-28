//! Batch steps on Kubernetes: the step runner's [`ContainerRuntime`] for `batches` jobs.
//!
//! The executor checks the workspace out locally and runs the step runner itself, as with
//! Docker; this runtime runs each step as its own Kubernetes Job (with a pod that copies the
//! working copy in and out), and resolves each image to a digest with a short Job of its own.
//!
//! # Why a Job per step with the working copy copied
//!
//! Between steps the runner runs `git` on the working copy (changed files, the cumulative
//! diff, cache resumes apply a diff), and `if:` and outputs of one step decide what the next
//! one runs. So steps need to run one at a time with the working copy visible to the executor
//! in between. The options were:
//!
//! - A volume shared by the executor and the step pods. It needs `ReadWriteMany` storage
//!   (absent from a plain k3s or most single-node installs), or step pods pinned to the
//!   executor's node with a `hostPath`/local volume (needs node access, fights the scheduler,
//!   `runtimeClassName` sandboxes and node selectors for job pods).
//! - One Job per workspace running every step and the git computations in a helper container
//!   in the pod: the whole step runner (templates, cache, outputs) inside the pod, a second
//!   protocol to drive it, and the cache and log plumbing duplicated.
//! - A pod per step with the working copy copied in and out: like the Docker runtime's
//!   [`WorkspaceMode::Copy`](sourcegull_batches_steps::docker::WorkspaceMode::Copy), it needs no
//!   shared storage, works on any cluster that runs the codeintel jobs, and keeps job pods
//!   free to be scheduled anywhere. The cost is two copies of the working copy per step
//!   through the API server, which is fine for the sizes bulk changes check out.
//!
//! This runtime takes the last.
//!
//! # A step's Job
//!
//! The pod has two `emptyDir` volumes: the working copy at [`WORKSPACE_DIR`] and the step's
//! script, files and mounts at [`STEP_DIR`].
//!
//! 1. The init container `copy-in` (the helper image) reads exactly the archive's size from
//!    its stdin (`head -c`, so no end of stream is needed) into `tar -x`, and makes everything
//!    writable for any user. It runs as the user the step image runs as (found when the image
//!    is resolved), so the working copy belongs to that user; `git` in a step then does not find the repository owned by someone
//!    else. The executor attaches to it
//!    and writes the archive: the working copy, the script, the step's `files` (each mounted
//!    at its path with `subPath`; files under [`WORKSPACE_DIR`] go into the working copy) and
//!    its `mount`s (likewise).
//! 2. The `step` container runs the image by digest. It waits for one line on its stdin
//!    before running the script with stdin closed, so the executor attaches first and no
//!    output is lost; attaching keeps stdout and stderr apart, which pod logs do not.
//! 3. The `helper` container sleeps; once the step has exited, the executor copies
//!    [`WORKSPACE_DIR`] out of it (`tar -c` over the exec API) over the host working copy.
//!
//! The step's environment (rendered `env` and the pass-through executor secrets) is a Secret
//! read with `envFrom`, so values never appear in the pod spec; the job's
//! `DOCKER_AUTH_CONFIG` credentials are a `kubernetes.io/dockerconfigjson` Secret in the pod's
//! `imagePullSecrets`. Both are owned by the Job. Step containers get the runtime's resources,
//! and pods its runtime class, node selector, tolerations and service account. The Job and its
//! Secrets are deleted when the step ends or is canceled; deadlines and TTLs cover executors
//! that die.
//!
//! # Image digests
//!
//! [`ContainerRuntime::resolve_image`] runs a Job whose container is the image itself with
//! `/bin/sh -c 'id -u; id -g'` and reads the container's `imageID` (a repository digest such
//! as `docker.io/library/alpine@sha256:…`) once the kubelet has pulled it, with the default
//! pull policy (`:latest` and untagged images are pulled again), and the user and group the
//! image runs as from its log (when the image cannot tell, `copy-in`
//! runs as the helper image's user). Steps then run by that digest. A
//! digest is reused for [`DIGEST_TTL`] by jobs with the same registry credentials, so a large
//! bulk change does not start a pod per workspace and image.
//!
//! # Images
//!
//! The helper image needs `sh`, `head`, `tar`, `find`, `chmod` and `sleep` (the Sourcegull image
//! and `alpine/git` have them); it runs as its own user, so files a step creates must be
//! readable by it (the usual `umask` makes them so). Step images need `/bin/sh`, as everywhere.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use k8s_openapi::api::batch::v1::Job as KubeJob;
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvFromSource, PodSpec, Secret, SecretEnvSource,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{AttachParams, LogParams};
use sourcegull_batches_steps::docker::{ID_SCRIPT, ImageUser, append_file, unpack_workspace};
use sourcegull_batches_steps::runtime::OutputFn;
use sourcegull_batches_steps::{
    ContainerRuntime, ContainerSpec, OutputStream, RuntimeError, WORKSPACE_DIR,
};
use sourcegull_executor_api::Job;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::{
    KubernetesOptions, KubernetesRuntime, Progress, Resources, job_name, labels, pod_job,
    registry_secret, step_resources, workspace_volume,
};
use crate::{JobContext, JobError};

/// Where the step volume (script, files, mounts) is mounted.
pub const STEP_DIR: &str = "/.sourcegull-step";
/// The step's script in [`STEP_DIR`].
const SCRIPT: &str = "step.sh";
/// How long a resolved image digest is reused.
pub const DIGEST_TTL: Duration = Duration::from_secs(5 * 60);

/// The step container's command (with the script as `$0`): waits until the executor has
/// attached and sent a line, then runs the script without stdin.
const STEP_WRAPPER: &str = r#"read -r _ || :; exec /bin/sh "$0" </dev/null"#;

/// The copy-in container's command (with the archive size as `$0`). `-o`: extracted files
/// belong to the user it runs as, also as root.
const COPY_IN: &str = r#"set -e
head -c "$0" | tar -x -o -f - -C /
find /work /.sourcegull-step -mindepth 1 ! -type l -exec chmod a+rwX {} +
"#;

fn other(what: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Other(format!("{what}: {e}"))
}

/// A Kubernetes runtime error as a step runtime error.
fn runtime_error(e: JobError, image: &str) -> RuntimeError {
    match e {
        JobError::Canceled => RuntimeError::Canceled,
        // Image pull failures (the only failures of waiting for a container).
        JobError::Failed(message) => RuntimeError::Image {
            image: image.to_owned(),
            message,
        },
        e => RuntimeError::Other(e.to_string()),
    }
}

/// The image reference a step runs: the digest when it is a repository digest, else the
/// image as written (an image only known by ID, e.g. imported into the node).
pub fn run_image(spec: &ContainerSpec) -> &str {
    if spec.digest.contains('@') {
        &spec.digest
    } else {
        &spec.image
    }
}

/// A container status's `imageID` as an image reference (Docker-based nodes prefix it).
pub fn image_reference(image_id: &str) -> &str {
    image_id
        .strip_prefix("docker-pullable://")
        .unwrap_or(image_id)
}

/// A `files` or `mount` target inside [`WORKSPACE_DIR`], as a path relative to it.
fn in_workspace(target: &str) -> Option<&str> {
    target
        .strip_prefix(WORKSPACE_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| rest.trim_start_matches('/'))
}

fn step_mount(target: &str, sub_path: String) -> VolumeMount {
    VolumeMount {
        name: "step".into(),
        mount_path: target.to_owned(),
        sub_path: Some(sub_path),
        ..Default::default()
    }
}

/// The Secrets and the Kubernetes Job of one step (see the module docs); `archive_size` is
/// the size of the archive [`pack`] wrote for it, `owner` the user the image runs as.
pub fn step_job(
    job: &Job,
    options: &KubernetesOptions,
    name: &str,
    executor: &str,
    spec: &ContainerSpec,
    archive_size: u64,
    owner: Option<ImageUser>,
) -> Result<(Vec<Secret>, KubeJob), RuntimeError> {
    let meta = ObjectMeta {
        name: Some(name.to_owned()),
        namespace: Some(options.namespace.clone()),
        labels: Some(labels(executor, name)),
        ..Default::default()
    };
    let mut secrets = Vec::new();
    if !spec.env.is_empty() {
        secrets.push(Secret {
            metadata: meta.clone(),
            string_data: Some(spec.env.clone().into_iter().collect()),
            type_: Some("Opaque".into()),
            ..Default::default()
        });
    }
    let pull_secrets = registry_secret(job, options, &meta, &mut secrets)
        .map_err(|e| RuntimeError::Other(e.to_string()))?;

    let work = VolumeMount {
        name: "work".into(),
        mount_path: WORKSPACE_DIR.into(),
        ..Default::default()
    };
    let step_dir = VolumeMount {
        name: "step".into(),
        mount_path: STEP_DIR.into(),
        ..Default::default()
    };
    let mut mounts = vec![
        work.clone(),
        VolumeMount {
            read_only: Some(true),
            ..step_dir.clone()
        },
    ];
    for (i, f) in spec.files.iter().enumerate() {
        if in_workspace(&f.target).is_none() {
            mounts.push(step_mount(&f.target, format!("files/{i}")));
        }
    }
    for (i, m) in spec.mounts.iter().enumerate() {
        if in_workspace(&m.target).is_none() {
            mounts.push(step_mount(&m.target, format!("mounts/{i}")));
        }
    }
    let copy_in = Container {
        name: "copy-in".into(),
        image: Some(options.helper_image.clone()),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            COPY_IN.into(),
            archive_size.to_string(),
        ]),
        stdin: Some(true),
        stdin_once: Some(true),
        volume_mounts: Some(vec![work.clone(), step_dir]),
        // What it extracts belongs to the step image's user.
        security_context: owner.map(|o| SecurityContext {
            run_as_user: Some(i64::from(o.uid)),
            run_as_group: Some(i64::from(o.gid)),
            run_as_non_root: (o.uid != 0).then_some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let step = Container {
        name: "step".into(),
        image: Some(run_image(spec).to_owned()),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            STEP_WRAPPER.into(),
            format!("{STEP_DIR}/{SCRIPT}"),
        ]),
        working_dir: Some(spec.workdir.clone()),
        env_from: (!spec.env.is_empty()).then(|| {
            vec![EnvFromSource {
                secret_ref: Some(SecretEnvSource {
                    name: name.to_owned(),
                    optional: None,
                }),
                ..Default::default()
            }]
        }),
        resources: step_resources(options),
        stdin: Some(true),
        stdin_once: Some(true),
        volume_mounts: Some(mounts),
        ..Default::default()
    };
    let helper = Container {
        name: "helper".into(),
        image: Some(options.helper_image.clone()),
        // Holds the pod open for the copy out; the executor deletes the Job then.
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "exec sleep \"$0\"".into(),
            options.deadline.as_secs().max(60).to_string(),
        ]),
        volume_mounts: Some(vec![work]),
        ..Default::default()
    };
    let pod = PodSpec {
        init_containers: Some(vec![copy_in]),
        containers: vec![step, helper],
        volumes: Some(vec![
            workspace_volume("work", options),
            Volume {
                name: "step".into(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    Ok((secrets, pod_job(options, meta, pod, pull_secrets)))
}

/// The Secrets and the Kubernetes Job that pull `image` and run it once, for its digest.
pub fn resolve_job(
    job: &Job,
    options: &KubernetesOptions,
    name: &str,
    executor: &str,
    image: &str,
) -> Result<(Vec<Secret>, KubeJob), RuntimeError> {
    let meta = ObjectMeta {
        name: Some(name.to_owned()),
        namespace: Some(options.namespace.clone()),
        labels: Some(labels(executor, name)),
        ..Default::default()
    };
    let mut secrets = Vec::new();
    let pull_secrets = registry_secret(job, options, &meta, &mut secrets)
        .map_err(|e| RuntimeError::Other(e.to_string()))?;
    let pod = PodSpec {
        containers: vec![Container {
            name: "resolve".into(),
            image: Some(image.to_owned()),
            command: Some(vec!["/bin/sh".into(), "-c".into(), ID_SCRIPT.into()]),
            ..Default::default()
        }],
        ..Default::default()
    };
    Ok((secrets, pod_job(options, meta, pod, pull_secrets)))
}

/// Writes the archive the copy-in container extracts at `/`: the working copy's entries
/// under `work/` (not the directory itself, which is the volume's root), the script and the
/// step's files and mounts under `.sourcegull-step/`, or under `work/` for targets inside it.
pub fn pack(spec: &ContainerSpec, archive: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(archive)?;
    let mut b = tar::Builder::new(std::io::BufWriter::new(file));
    b.follow_symlinks(false);
    let work = WORKSPACE_DIR.trim_start_matches('/');
    let step = STEP_DIR.trim_start_matches('/');
    let mut entries: Vec<_> = std::fs::read_dir(&spec.workspace)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = format!("{work}/{}", entry.file_name().to_string_lossy());
        if entry.file_type()?.is_dir() {
            b.append_dir_all(&name, &path)?;
        } else {
            b.append_path_with_name(&path, &name)?;
        }
    }
    append_file(
        &mut b,
        &format!("{step}/{SCRIPT}"),
        spec.script.as_bytes(),
        0o755,
    )?;
    for (i, f) in spec.files.iter().enumerate() {
        let path = match in_workspace(&f.target) {
            Some(rel) => format!("{work}/{rel}"),
            None => format!("{step}/files/{i}"),
        };
        append_file(&mut b, &path, &f.content, 0o644)?;
    }
    for (i, m) in spec.mounts.iter().enumerate() {
        let path = match in_workspace(&m.target) {
            Some(rel) => format!("{work}/{rel}"),
            None => format!("{step}/mounts/{i}"),
        };
        if m.source.is_dir() {
            b.append_dir_all(&path, &m.source)?;
        } else {
            b.append_path_with_name(&m.source, &path)?;
        }
    }
    b.into_inner()?.into_inner().map_err(|e| e.into_error())?;
    Ok(())
}

/// Runs the batch steps of one job on Kubernetes (see the module docs).
pub struct KubernetesSteps {
    kube: KubernetesRuntime,
    job: Job,
    executor: String,
    /// Prefix of the names of the job's Kubernetes Jobs.
    base: String,
    /// Where archives are staged.
    scratch: PathBuf,
    next: AtomicUsize,
    /// The user each resolved image (by digest) runs as.
    owners: std::sync::Mutex<std::collections::HashMap<String, ImageUser>>,
}

impl KubernetesSteps {
    /// The steps of `job`; archives are staged in `scratch`.
    pub fn new(kube: KubernetesRuntime, job: &Job, ctx: &JobContext, scratch: PathBuf) -> Self {
        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        Self {
            kube,
            job: job.clone(),
            executor: ctx.executor.clone(),
            base: job_name(job, &suffix),
            scratch,
            next: AtomicUsize::new(0),
            owners: Default::default(),
        }
    }

    fn name(&self, what: &str) -> String {
        format!(
            "{}-{what}{}",
            self.base,
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The digest cache's key: the image and the credentials it was pulled with.
    fn digest_key(&self, image: &str) -> String {
        let auth = self
            .job
            .docker_auth_config
            .as_ref()
            .and_then(|c| serde_json::to_string(c).ok())
            .unwrap_or_default();
        format!("{image}\n{auth}")
    }

    async fn resolve_inner(
        &self,
        image: &str,
        name: &str,
        res: &mut Resources,
        cancel: &CancellationToken,
    ) -> Result<(String, Option<ImageUser>), RuntimeError> {
        let (secrets, kube_job) =
            resolve_job(&self.job, &self.kube.options, name, &self.executor, image)?;
        let err = |e: JobError| runtime_error(e, image);
        self.kube
            .create_job(&secrets, &kube_job, res)
            .await
            .map_err(err)?;
        let pod = self.kube.find_pod(name, cancel).await.map_err(err)?;
        loop {
            let progress = self
                .kube
                .wait_started(&pod, "resolve", cancel)
                .await
                .map_err(err)?;
            let Progress::Terminated(code) = progress else {
                self.kube.sleep(cancel).await.map_err(err)?;
                continue;
            };
            let p = self
                .kube
                .pods()
                .get(&pod)
                .await
                .map_err(|e| other("reading the pod", e))?;
            let id = p
                .status
                .and_then(|s| s.container_statuses)
                .into_iter()
                .flatten()
                .find(|s| s.name == "resolve")
                .map(|s| s.image_id)
                .unwrap_or_default();
            if id.is_empty() {
                return Err(RuntimeError::Image {
                    image: image.to_owned(),
                    message: "the container ran but reported no image ID".into(),
                });
            }
            let owner = if code == 0 {
                let params = LogParams {
                    container: Some("resolve".into()),
                    ..Default::default()
                };
                match self.kube.pods().logs(&pod, &params).await {
                    Ok(log) => ImageUser::from_id_output(&log),
                    Err(e) => {
                        tracing::debug!(error = %e, pod, "reading the resolve log failed");
                        None
                    }
                }
            } else {
                None
            };
            if owner.is_none() {
                tracing::warn!(
                    image,
                    "could not determine the image's user; the working copy keeps the helper's"
                );
            }
            return Ok((image_reference(&id).to_owned(), owner));
        }
    }

    async fn run_inner(
        &self,
        spec: &ContainerSpec,
        name: &str,
        archive: &Path,
        res: &mut Resources,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        let size = {
            let (spec, archive) = (spec.clone(), archive.to_owned());
            tokio::task::spawn_blocking(move || {
                pack(&spec, &archive)?;
                std::fs::metadata(&archive).map(|m| m.len())
            })
            .await
            .map_err(|e| other("packing the step's files", e))?
            .map_err(|e| other("packing the step's files", e))?
        };
        let owner = self
            .owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&spec.digest)
            .copied();
        let (secrets, kube_job) = step_job(
            &self.job,
            &self.kube.options,
            name,
            &self.executor,
            spec,
            size,
            owner,
        )?;
        let err = |e: JobError| runtime_error(e, &spec.image);
        self.kube
            .create_job(&secrets, &kube_job, res)
            .await
            .map_err(err)?;
        let pod = self.kube.find_pod(name, cancel).await.map_err(err)?;

        // Copy in.
        if let Progress::Terminated(code) = self
            .kube
            .wait_started(&pod, "copy-in", cancel)
            .await
            .map_err(err)?
        {
            return Err(RuntimeError::Other(format!(
                "the copy-in container exited with status {code} before the copy"
            )));
        }
        let errors = self.copy_in(&pod, archive).await?;
        match self.wait_terminated(&pod, "copy-in", cancel).await? {
            0 => {}
            code => {
                return Err(RuntimeError::Other(format!(
                    "copying the working copy in failed with status {code}: {}",
                    String::from_utf8_lossy(&errors).trim()
                )));
            }
        }

        // Run the step.
        match self
            .kube
            .wait_started(&pod, "step", cancel)
            .await
            .map_err(err)?
        {
            // It could not start (e.g. no /bin/sh); its output is in its logs.
            Progress::Terminated(code) => return Ok(code),
            _ => self.attach_step(&pod, output).await?,
        }
        let code = self.wait_terminated(&pod, "step", cancel).await?;

        // Copy out.
        self.kube
            .exec_to_file(
                &pod,
                "helper",
                &[
                    "tar",
                    "-c",
                    "-f",
                    "-",
                    "-C",
                    "/",
                    WORKSPACE_DIR.trim_start_matches('/'),
                ],
                archive,
                "copying the working copy out",
            )
            .await
            .map_err(|e| RuntimeError::Other(e.to_string()))?;
        let (archive, workspace) = (archive.to_owned(), spec.workspace.clone());
        tokio::task::spawn_blocking(move || unpack_workspace(&archive, &workspace))
            .await
            .map_err(|e| other("copying the working copy out", e))?
            .map_err(|e| other("copying the working copy out", e))?;
        Ok(code)
    }

    /// Writes the archive to the copy-in container's stdin; returns what it wrote to stderr.
    async fn copy_in(&self, pod: &str, archive: &Path) -> Result<Vec<u8>, RuntimeError> {
        let what = "copying the working copy in";
        let mut attached = self
            .kube
            .pods()
            .attach(
                pod,
                &AttachParams::default()
                    .container("copy-in")
                    .stdin(true)
                    .stdout(true)
                    .stderr(true)
                    .max_stdin_buf_size(64 * 1024),
            )
            .await
            .map_err(|e| other(what, e))?;
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| RuntimeError::Other(format!("{what}: no input stream")))?;
        let (stdout, stderr) = (attached.stdout(), attached.stderr());
        let write = async {
            let mut file = tokio::fs::File::open(archive).await?;
            tokio::io::copy(&mut file, &mut stdin).await?;
            stdin.flush().await
        };
        // Keep stdin open until the container is done: closing it may close the connection.
        let (written, _, errors) = tokio::join!(write, read_all(stdout), read_all(stderr));
        drop(stdin);
        let joined = attached.join().await;
        written.map_err(|e| other(what, e))?;
        joined.map_err(|e| other(what, e))?;
        Ok(errors)
    }

    /// Attaches to the waiting step container, lets it start and streams its output until it
    /// exits.
    async fn attach_step(&self, pod: &str, output: OutputFn<'_>) -> Result<(), RuntimeError> {
        let what = "attaching to the step container";
        let mut attached = self
            .kube
            .pods()
            .attach(
                pod,
                &AttachParams::default()
                    .container("step")
                    .stdin(true)
                    .stdout(true)
                    .stderr(true)
                    .max_stdout_buf_size(64 * 1024)
                    .max_stderr_buf_size(64 * 1024),
            )
            .await
            .map_err(|e| other(what, e))?;
        let mut stdin = attached
            .stdin()
            .ok_or_else(|| RuntimeError::Other(format!("{what}: no input stream")))?;
        stdin.write_all(b"\n").await.map_err(|e| other(what, e))?;
        stdin.flush().await.map_err(|e| other(what, e))?;
        tokio::join!(
            pump(attached.stdout(), OutputStream::Stdout, output),
            pump(attached.stderr(), OutputStream::Stderr, output),
        );
        drop(stdin);
        if let Err(e) = attached.join().await {
            tracing::debug!(error = %e, pod, "the step's attach session ended with an error");
        }
        Ok(())
    }

    /// Polls until `container` has exited and returns its exit code.
    async fn wait_terminated(
        &self,
        pod: &str,
        container: &str,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        loop {
            match self.kube.wait_started(pod, container, cancel).await {
                Ok(Progress::Terminated(code)) => return Ok(code),
                Ok(_) => {}
                Err(e) => return Err(runtime_error(e, container)),
            }
            self.kube
                .sleep(cancel)
                .await
                .map_err(|e| runtime_error(e, container))?;
        }
    }
}

async fn read_all(r: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut r) = r {
        let _ = r.read_to_end(&mut buf).await;
    }
    buf
}

async fn pump(r: Option<impl AsyncRead + Unpin>, stream: OutputStream, output: OutputFn<'_>) {
    let Some(mut r) = r else {
        return;
    };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => output(stream, &buf[..n]),
            Err(e) => {
                tracing::debug!(error = %e, "reading the step's output failed");
                return;
            }
        }
    }
}

#[async_trait]
impl ContainerRuntime for KubernetesSteps {
    async fn resolve_image(
        &self,
        image: &str,
        cancel: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        let key = self.digest_key(image);
        let remember = |digest: &str, owner: Option<ImageUser>| {
            if let Some(o) = owner {
                self.owners
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(digest.to_owned(), o);
            }
        };
        {
            let mut digests = self.kube.digests.lock().unwrap_or_else(|e| e.into_inner());
            digests.retain(|_, (_, _, at)| at.elapsed() < DIGEST_TTL);
            if let Some((digest, owner, _)) = digests.get(&key) {
                remember(digest, *owner);
                return Ok(digest.clone());
            }
        }
        let name = self.name("image");
        let mut res = Resources::default();
        let result = tokio::select! {
            r = self.resolve_inner(image, &name, &mut res, cancel) => r,
            _ = cancel.cancelled() => Err(RuntimeError::Canceled),
        };
        self.kube.cleanup(res).await;
        let (digest, owner) = result?;
        remember(&digest, owner);
        self.kube
            .digests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (digest.clone(), owner, Instant::now()));
        Ok(digest)
    }

    async fn run(
        &self,
        spec: &ContainerSpec,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        let name = self.name(&format!("step{}-", spec.step));
        let archive = self.scratch.join(format!("{name}.tar"));
        let mut res = Resources::default();
        let result = tokio::select! {
            r = self.run_inner(spec, &name, &archive, &mut res, output, cancel) => r,
            _ = cancel.cancelled() => Err(RuntimeError::Canceled),
        };
        self.kube.cleanup(res).await;
        let _ = tokio::fs::remove_file(&archive).await;
        result
    }
}

impl KubernetesRuntime {
    /// Runs a `batches` job with the step runner on [`KubernetesSteps`].
    pub(super) async fn run_batches(&self, job: &Job, ctx: &JobContext) -> Result<(), JobError> {
        let archives = ctx.scratch.join("archives");
        tokio::fs::create_dir_all(&archives)
            .await
            .map_err(|e| JobError::Errored(format!("creating the scratch directory: {e}")))?;
        let steps = KubernetesSteps::new(self.clone(), job, ctx, archives);
        crate::batches::run_job(job, ctx, std::sync::Arc::new(steps)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_batches_steps::{ContainerFile, HostMount};
    use sourcegull_executor_api::{DockerAuthConfig, DockerAuthConfigAuth};

    fn job() -> Job {
        Job {
            id: 7,
            queue: "batches".into(),
            token: "sjt_secret".into(),
            docker_auth_config: Some(DockerAuthConfig {
                auths: [(
                    "registry.example.com".to_owned(),
                    DockerAuthConfigAuth {
                        auth: "dXNlcjpwYXNz".into(),
                    },
                )]
                .into(),
            }),
            ..Default::default()
        }
    }

    fn spec(workspace: &Path) -> ContainerSpec {
        ContainerSpec {
            step: 1,
            image: "alpine:3".into(),
            digest: "docker.io/library/alpine@sha256:abc".into(),
            workspace: workspace.to_owned(),
            workdir: "/work/docs".into(),
            script: "echo hi\n".into(),
            env: [
                ("TOKEN".to_owned(), "hunter2-value".to_owned()),
                ("PLAIN".to_owned(), "x".to_owned()),
            ]
            .into(),
            files: vec![
                ContainerFile {
                    target: "/tmp/input.txt".into(),
                    content: b"input".to_vec(),
                },
                ContainerFile {
                    target: "/work/docs/new.txt".into(),
                    content: b"new".to_vec(),
                },
            ],
            mounts: vec![HostMount {
                source: workspace.join("README.md"),
                target: "/opt/readme".into(),
            }],
        }
    }

    #[test]
    fn plans_a_step_job() {
        let dir = tempfile::tempdir().unwrap();
        let options = KubernetesOptions {
            namespace: "jobs".into(),
            helper_image: "sourcegull/sourcegull:1".into(),
            runtime_class: Some("gvisor".into()),
            node_selector: [("pool".to_owned(), "executors".to_owned())].into(),
            image_pull_secrets: vec!["shared".into()],
            memory_limit: Some("1Gi".into()),
            cpu_request: Some("250m".into()),
            workspace_size: Some("2Gi".into()),
            ..Default::default()
        };
        let (secrets, kjob) = step_job(
            &job(),
            &options,
            "sourcegull-batches-7-x-step1-0",
            "exec 1",
            &spec(dir.path()),
            4096,
            Some(ImageUser {
                uid: 1000,
                gid: 1001,
            }),
        )
        .unwrap();

        // The environment and the registry credentials live in Secrets only.
        assert_eq!(secrets.len(), 2);
        let env = secrets[0].string_data.as_ref().unwrap();
        assert_eq!(env["TOKEN"], "hunter2-value");
        assert_eq!(
            secrets[1].type_.as_deref(),
            Some("kubernetes.io/dockerconfigjson")
        );
        let json = serde_json::to_string(&kjob).unwrap();
        assert!(!json.contains("hunter2-value"));
        assert!(!json.contains("dXNlcjpwYXNz"));
        assert!(!json.contains("sjt_secret"));

        let spec_ = kjob.spec.unwrap();
        assert_eq!(spec_.backoff_limit, Some(0));
        assert_eq!(
            spec_.template.metadata.unwrap().labels.unwrap()[super::super::JOB_LABEL],
            "sourcegull-batches-7-x-step1-0"
        );
        let pod = spec_.template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("gvisor"));
        assert_eq!(pod.node_selector.unwrap()["pool"], "executors");
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        let pulls: Vec<_> = pod
            .image_pull_secrets
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(pulls, ["sourcegull-batches-7-x-step1-0-registry", "shared"]);
        let volumes = pod.volumes.unwrap();
        assert_eq!(
            volumes[0]
                .empty_dir
                .as_ref()
                .unwrap()
                .size_limit
                .as_ref()
                .unwrap()
                .0,
            "2Gi"
        );

        let init = pod.init_containers.unwrap();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].image.as_deref(), Some("sourcegull/sourcegull:1"));
        assert_eq!(init[0].stdin, Some(true));
        assert_eq!(init[0].command.as_ref().unwrap()[3], "4096");
        // It extracts as the step image's user.
        let sc = init[0].security_context.as_ref().unwrap();
        assert_eq!((sc.run_as_user, sc.run_as_group), (Some(1000), Some(1001)));

        let [step, helper] = &pod.containers[..] else {
            panic!("two containers")
        };
        assert_eq!(step.name, "step");
        assert_eq!(
            step.image.as_deref(),
            Some("docker.io/library/alpine@sha256:abc")
        );
        assert_eq!(step.working_dir.as_deref(), Some("/work/docs"));
        assert_eq!(step.stdin, Some(true));
        assert_eq!(step.stdin_once, Some(true));
        assert_eq!(
            step.command.as_ref().unwrap()[3],
            "/.sourcegull-step/step.sh"
        );
        assert!(step.env.is_none());
        let from = step.env_from.as_ref().unwrap()[0]
            .secret_ref
            .as_ref()
            .unwrap();
        assert_eq!(from.name, "sourcegull-batches-7-x-step1-0");
        let resources = step.resources.as_ref().unwrap();
        assert_eq!(resources.limits.as_ref().unwrap()["memory"].0, "1Gi");
        assert_eq!(resources.requests.as_ref().unwrap()["cpu"].0, "250m");
        let mounts: Vec<_> = step
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| (m.mount_path.as_str(), m.sub_path.as_deref()))
            .collect();
        // The file under /work goes into the working copy instead.
        assert_eq!(
            mounts,
            [
                ("/work", None),
                ("/.sourcegull-step", None),
                ("/tmp/input.txt", Some("files/0")),
                ("/opt/readme", Some("mounts/0")),
            ]
        );
        assert_eq!(helper.image.as_deref(), Some("sourcegull/sourcegull:1"));
        assert!(helper.resources.is_none());
    }

    #[test]
    fn steps_without_env_or_credentials_need_no_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path());
        s.env.clear();
        s.digest = "sha256:abc".into();
        let mut j = job();
        j.docker_auth_config = None;
        let (secrets, kjob) =
            step_job(&j, &KubernetesOptions::default(), "n", "e", &s, 1, None).unwrap();
        assert!(secrets.is_empty());
        let pod = kjob.spec.unwrap().template.spec.unwrap();
        assert!(pod.image_pull_secrets.is_none());
        // An image known only by ID runs as written.
        assert_eq!(pod.containers[0].image.as_deref(), Some("alpine:3"));
        assert!(pod.containers[0].env_from.is_none());
    }

    #[test]
    fn plans_a_resolve_job() {
        let (secrets, kjob) = resolve_job(
            &job(),
            &KubernetesOptions::default(),
            "n-image0",
            "e",
            "alpine:3",
        )
        .unwrap();
        assert_eq!(secrets.len(), 1);
        let pod = kjob.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.containers[0].image.as_deref(), Some("alpine:3"));
        assert_eq!(pod.image_pull_secrets.unwrap()[0].name, "n-image0-registry");
        assert_eq!(
            image_reference("docker-pullable://alpine@sha256:abc"),
            "alpine@sha256:abc"
        );
        assert_eq!(
            image_reference("docker.io/library/alpine@sha256:abc"),
            "docker.io/library/alpine@sha256:abc"
        );
    }

    #[test]
    fn packs_the_step() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("docs")).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("README.md"), "readme").unwrap();
        std::fs::write(ws.join("docs/guide.md"), "guide").unwrap();
        let archive = dir.path().join("a.tar");
        pack(&spec(&ws), &archive).unwrap();
        let mut tar = tar::Archive::new(std::fs::File::open(&archive).unwrap());
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        for want in [
            "work/.git",
            "work/README.md",
            "work/docs/guide.md",
            ".sourcegull-step/step.sh",
            ".sourcegull-step/files/0",
            "work/docs/new.txt",
            ".sourcegull-step/mounts/0",
        ] {
            assert!(
                names.iter().any(|n| n.trim_end_matches('/') == want),
                "{want} in {names:?}"
            );
        }
        // Never the volume roots themselves.
        assert!(!names.iter().any(|n| n == "work" || n == "work/"));
        assert!(!names.iter().any(|n| n == ".sourcegull-step/"));
    }
}
