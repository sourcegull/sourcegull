//! The Docker runtime: one Docker volume per job as the shared workspace, one
//! container per step.
//!
//! The executor checks the commit out locally, copies it into the job's volume through a
//! helper container that is created but never started, runs each step in its own container
//! with the volume at `/data` (the repository at `/data/repository`, step scripts under
//! `/data/.sourcegull-executor/`), and copies the index back out of the volume for the upload.
//! A volume rather than a bind mount keeps this working when the executor itself runs in a
//! container with the Docker socket mounted, and leaves no root-owned files behind.
//!
//! Executor secrets are added to every step container's environment, and the job's
//! `DOCKER_AUTH_CONFIG` credentials authenticate image pulls from the matching registry.
//!
//! Jobs of the `batches` queue run with the batch step runner ([`crate::batches`]) on its
//! Docker runtime in [`WorkspaceMode::Copy`]: the working copy is copied into each step's
//! container and back, which, like the job volume here, works when the executor runs in a
//! container with the Docker socket mounted. Step containers carry this runtime's labels and
//! limits; only pass-through secrets (`env: [NAME]`) reach a step.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountTypeEnum, VolumeCreateOptions};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder,
    KillContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveVolumeOptionsBuilder, UploadToContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures::StreamExt;
/// Shared with the batch step runner's Docker runtime.
pub use sourcegull_batches_steps::docker::split_image;
use sourcegull_batches_steps::docker::{
    DockerOptions as StepsDockerOptions, DockerRuntime as StepsDockerRuntime, WorkspaceMode,
};
use sourcegull_executor_api::{DockerAuthConfig, DockerStep, Job, capability};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::checkout::checkout;
use crate::logger::{JobLogger, LineBuffer, StepLog, decode_auth};
use crate::{JobContext, JobError, Runtime};

/// Where the job's volume is mounted in step containers.
const DATA: &str = "/data";
/// The repository inside the volume.
const REPO_DIR: &str = "repository";
/// Step scripts inside the volume.
const SCRIPTS_DIR: &str = ".sourcegull-executor";
/// Label marking everything the executor creates.
const LABEL: &str = "io.sourcegull.executor";

/// Settings of the Docker runtime.
#[derive(Debug, Clone)]
pub struct DockerOptions {
    /// Network of step containers: `bridge` (default), `none` or a named network (for
    /// example one whose only egress is an allowlisting proxy).
    pub network: String,
    /// Memory limit per step container in bytes (0: none).
    pub memory_bytes: i64,
    /// CPU limit per step container (0: none).
    pub cpus: f64,
    /// Pull images that are already present (to pick up moved tags).
    pub always_pull: bool,
}

impl Default for DockerOptions {
    fn default() -> Self {
        Self {
            network: "bridge".into(),
            memory_bytes: 0,
            cpus: 0.0,
            always_pull: false,
        }
    }
}

/// Runs jobs with a Docker daemon.
pub struct DockerRuntime {
    docker: Docker,
    options: DockerOptions,
}

impl DockerRuntime {
    /// Connects with the usual defaults (`DOCKER_HOST`, else the local socket).
    pub fn connect(options: DockerOptions) -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_defaults()?,
            options,
        })
    }
}

fn errored(what: &str, e: impl std::fmt::Display) -> JobError {
    JobError::Errored(format!("{what}: {e}"))
}

/// The script of a step: its commands in order, stopping at the first failure.
pub fn step_script(step: &DockerStep) -> String {
    let mut script = String::from("set -e\n");
    for c in &step.commands {
        script.push_str(c);
        script.push('\n');
    }
    script
}

/// The working directory of a step in its container.
pub fn step_workdir(step: &DockerStep) -> Result<String, JobError> {
    let mut dir = format!("{DATA}/{REPO_DIR}");
    for seg in step.dir.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            return Err(JobError::Failed(format!(
                "step {} has a directory outside the repository: {:?}",
                step.key, step.dir
            )));
        }
        dir.push('/');
        dir.push_str(seg);
    }
    Ok(dir)
}

/// A path relative to the repository, checked to stay inside it.
pub(crate) fn repo_path(rel: &str) -> Result<String, JobError> {
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return Err(JobError::Failed(format!("invalid file path {rel:?}")));
    }
    Ok(format!("{DATA}/{REPO_DIR}/{}", parts.join("/")))
}

/// Everything created for one job, removed by [`DockerRuntime::cleanup`].
#[derive(Default)]
struct Resources {
    volume: Option<String>,
    containers: Vec<String>,
}

#[async_trait]
impl Runtime for DockerRuntime {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn version(&self) -> String {
        self.docker
            .version()
            .await
            .ok()
            .and_then(|v| v.version)
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

impl DockerRuntime {
    /// Runs a `batches` job with the step runner's Docker runtime.
    async fn run_batches(&self, job: &Job, ctx: &JobContext) -> Result<(), JobError> {
        let archives = ctx.scratch.join("archives");
        tokio::fs::create_dir_all(&archives)
            .await
            .map_err(|e| errored("creating the scratch directory", e))?;
        let auth = job.docker_auth_config.clone();
        let steps = StepsDockerRuntime::new(
            self.docker.clone(),
            StepsDockerOptions {
                workspace_mode: WorkspaceMode::Copy,
                network: self.options.network.clone(),
                memory_bytes: self.options.memory_bytes,
                cpus: self.options.cpus,
                always_pull: self.options.always_pull,
                labels: job_labels(job, ctx),
            },
        )
        .with_credentials(Arc::new(move |image: &str| {
            auth.as_ref().and_then(|c| registry_credentials(c, image))
        }))
        .with_scratch_dir(archives);
        crate::batches::run_job(job, ctx, Arc::new(steps)).await
    }

    async fn run_inner(
        &self,
        job: &Job,
        ctx: &JobContext,
        res: &mut Resources,
    ) -> Result<(), JobError> {
        let logger = &ctx.logger;
        let scratch = ctx.scratch.as_path();
        let labels = job_labels(job, ctx);

        // Images first: a missing image fails fast, before the checkout.
        let mut images: Vec<&str> = job.docker_steps.iter().map(|s| s.image.as_str()).collect();
        images.sort_unstable();
        images.dedup();
        for image in &images {
            let credentials = job
                .docker_auth_config
                .as_ref()
                .and_then(|c| registry_credentials(c, image));
            self.ensure_image(image, credentials, logger, &ctx.cancel)
                .await?;
        }

        // Check out locally, add the step scripts, and pack it all for the volume.
        let local = scratch.join("workspace");
        checkout(
            &ctx.git,
            &ctx.client,
            job,
            &local.join(REPO_DIR),
            logger,
            &ctx.cancel,
        )
        .await?;
        let scripts = local.join(SCRIPTS_DIR);
        tokio::fs::create_dir_all(&scripts)
            .await
            .map_err(|e| errored("writing step scripts", e))?;
        for (i, step) in job.docker_steps.iter().enumerate() {
            tokio::fs::write(scripts.join(format!("{i}.sh")), step_script(step))
                .await
                .map_err(|e| errored("writing step scripts", e))?;
        }
        let Some(helper_image) = images.first() else {
            // Nothing runs in a container: the upload reads the local checkout.
            return self.upload(job, ctx, None, &local).await;
        };
        let tarball = scratch.join("workspace.tar");
        {
            let (local, tarball) = (local.clone(), tarball.clone());
            tokio::task::spawn_blocking(move || pack(&local, &tarball))
                .await
                .map_err(|e| errored("packing the workspace", e))?
                .map_err(|e| errored("packing the workspace", e))?;
        }
        let _ = tokio::fs::remove_dir_all(&local).await;

        let volume = format!(
            "sourcegull-executor-{}-{}-{}",
            job.queue,
            job.id,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        self.docker
            .create_volume(VolumeCreateOptions {
                name: Some(volume.clone()),
                labels: Some(labels.clone()),
                ..Default::default()
            })
            .await
            .map_err(|e| errored("creating the workspace volume", e))?;
        res.volume = Some(volume.clone());

        // The helper container gives access to the volume without running anything.
        let helper = self
            .create(
                &format!("{volume}-files"),
                ContainerCreateBody {
                    image: Some((*helper_image).to_owned()),
                    entrypoint: Some(vec!["/bin/sh".into()]),
                    labels: Some(labels.clone()),
                    host_config: Some(HostConfig {
                        mounts: Some(vec![volume_mount(&volume)]),
                        network_mode: Some("none".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await?;
        res.containers.push(helper.clone());
        let file = tokio::fs::File::open(&tarball)
            .await
            .map_err(|e| errored("reading the workspace archive", e))?;
        let body = tokio_util::io::ReaderStream::new(file);
        self.docker
            .upload_to_container(
                &helper,
                Some(UploadToContainerOptionsBuilder::new().path(DATA).build()),
                bollard::body_try_stream(body),
            )
            .await
            .map_err(|e| errored("copying the workspace into the volume", e))?;
        let _ = tokio::fs::remove_file(&tarball).await;

        for (i, step) in job.docker_steps.iter().enumerate() {
            let workdir = step_workdir(step)?;
            let name = format!("{volume}-{i}");
            let body = ContainerCreateBody {
                image: Some(step.image.clone()),
                entrypoint: Some(vec!["/bin/sh".into()]),
                cmd: Some(vec![format!("{DATA}/{SCRIPTS_DIR}/{i}.sh")]),
                working_dir: Some(workdir.clone()),
                env: Some(step_env(job, step)),
                labels: Some(labels.clone()),
                host_config: Some(HostConfig {
                    mounts: Some(vec![volume_mount(&volume)]),
                    network_mode: Some(self.options.network.clone()),
                    memory: (self.options.memory_bytes > 0).then_some(self.options.memory_bytes),
                    nano_cpus: (self.options.cpus > 0.0)
                        .then_some((self.options.cpus * 1e9) as i64),
                    init: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let mut command = vec![
                "docker".to_owned(),
                "run".to_owned(),
                "--workdir".to_owned(),
                workdir,
                step.image.clone(),
            ];
            command.extend(step.commands.iter().cloned());
            let log = logger.start(&step.key, &command).await?;
            let id = match self.create(&name, body).await {
                Ok(id) => id,
                Err(e) => {
                    log.line(&e.to_string());
                    log.finish(Some(-1)).await?;
                    return Err(e);
                }
            };
            res.containers.push(id.clone());
            let code = self.run_container(&id, &log, &ctx.cancel).await;
            match code {
                Ok(code) => {
                    log.finish(Some(code)).await?;
                    if code != 0 {
                        return Err(JobError::Failed(format!(
                            "step {} exited with status {code}",
                            step.key
                        )));
                    }
                }
                Err(e) => {
                    log.line(&e.to_string());
                    log.finish(None).await?;
                    return Err(e);
                }
            }
            // Free the step's container right away; the volume keeps its files.
            self.remove_container(&id).await;
            res.containers.retain(|c| *c != id);
        }

        self.upload(job, ctx, Some(&helper), scratch).await
    }

    /// Uploads the job's index, copied out of the volume (through `helper`) or read from the
    /// local checkout in `dir`.
    async fn upload(
        &self,
        job: &Job,
        ctx: &JobContext,
        helper: Option<&str>,
        dir: &Path,
    ) -> Result<(), JobError> {
        let Some(upload) = &job.upload else {
            return Ok(());
        };
        let log = ctx
            .logger
            .start(&upload.key, &["upload".to_owned(), upload.file.clone()])
            .await?;
        let result: Result<(), JobError> = async {
            let in_volume = repo_path(&upload.file)?;
            let local: PathBuf = match helper {
                Some(helper) => {
                    let dest = dir.join("index.upload");
                    self.copy_out(helper, &in_volume, &dest).await?;
                    dest
                }
                None => dir.join(in_volume.trim_start_matches(&format!("{DATA}/"))),
            };
            let size = tokio::fs::metadata(&local)
                .await
                .map_err(|_| JobError::Failed(format!("the indexer wrote no {}", upload.file)))?
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
        .await;
        match &result {
            Ok(()) => log.finish(Some(0)).await?,
            Err(e) => {
                log.line(&e.to_string());
                log.finish(Some(1)).await?;
            }
        }
        result
    }

    /// Copies one file out of a container to `dest`.
    async fn copy_out(&self, container: &str, path: &str, dest: &Path) -> Result<(), JobError> {
        let tar_path = dest.with_extension("tar");
        {
            let mut stream = self.docker.download_from_container(
                container,
                Some(
                    DownloadFromContainerOptionsBuilder::new()
                        .path(path)
                        .build(),
                ),
            );
            let mut f = tokio::fs::File::create(&tar_path)
                .await
                .map_err(|e| errored("copying the index", e))?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| match e {
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: 404, ..
                    } => JobError::Failed(format!(
                        "the indexer wrote no {}",
                        path.trim_start_matches(&format!("{DATA}/{REPO_DIR}/"))
                    )),
                    e => errored("copying the index", e),
                })?;
                f.write_all(&chunk)
                    .await
                    .map_err(|e| errored("copying the index", e))?;
            }
            f.flush()
                .await
                .map_err(|e| errored("copying the index", e))?;
        }
        let dest = dest.to_owned();
        let result = tokio::task::spawn_blocking(move || unpack_single(&tar_path, &dest))
            .await
            .map_err(|e| errored("copying the index", e))?;
        result.map_err(|e| JobError::Failed(format!("copying the index: {e}")))
    }

    async fn ensure_image(
        &self,
        image: &str,
        credentials: Option<DockerCredentials>,
        logger: &JobLogger,
        cancel: &CancellationToken,
    ) -> Result<(), JobError> {
        if !self.options.always_pull && self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        let log = logger
            .start(
                &format!("setup.docker.pull.{image}"),
                &["docker".to_owned(), "pull".to_owned(), image.to_owned()],
            )
            .await?;
        let (name, tag) = split_image(image);
        let mut stream = self.docker.create_image(
            Some(
                CreateImageOptionsBuilder::new()
                    .from_image(name)
                    .tag(tag)
                    .build(),
            ),
            None,
            credentials,
        );
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = cancel.cancelled() => {
                    log.finish(None).await?;
                    return Err(JobError::Canceled);
                }
            };
            match item {
                None => break,
                Some(Ok(info)) => {
                    if let Some(status) = info.status
                        && info.progress_detail.is_none_or(|p| p.total.is_none())
                    {
                        let id = info.id.map(|id| format!("{id}: ")).unwrap_or_default();
                        log.line(&format!("{id}{status}"));
                    }
                }
                Some(Err(e)) => {
                    log.line(&e.to_string());
                    log.finish(Some(1)).await?;
                    return Err(JobError::Failed(format!("pulling {image}: {e}")));
                }
            }
        }
        log.finish(Some(0)).await?;
        Ok(())
    }

    async fn create(&self, name: &str, body: ContainerCreateBody) -> Result<String, JobError> {
        let resp = self
            .docker
            .create_container(
                Some(CreateContainerOptionsBuilder::new().name(name).build()),
                body,
            )
            .await
            .map_err(|e| errored("creating a container", e))?;
        Ok(resp.id)
    }

    /// Starts a container, streams its output into `log` and returns its exit code. Killed
    /// on cancellation.
    async fn run_container(
        &self,
        id: &str,
        log: &StepLog,
        cancel: &CancellationToken,
    ) -> Result<i32, JobError> {
        self.docker
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .map_err(|e| errored("starting a container", e))?;
        let mut logs = self.docker.logs(
            id,
            Some(
                LogsOptionsBuilder::new()
                    .follow(true)
                    .stdout(true)
                    .stderr(true)
                    .build(),
            ),
        );
        // Output is logged by whole lines, one buffer per stream.
        let (mut stdout, mut stderr) = (LineBuffer::default(), LineBuffer::default());
        loop {
            tokio::select! {
                item = logs.next() => match item {
                    Some(Ok(out)) => {
                        let buf = match out {
                            LogOutput::StdErr { .. } => &mut stderr,
                            _ => &mut stdout,
                        };
                        if let Some(text) = buf.push(out.as_ref()) {
                            log.append(&text);
                        }
                    }
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "reading container logs failed");
                        break;
                    }
                    None => break,
                },
                _ = cancel.cancelled() => {
                    let _ = self
                        .docker
                        .kill_container(id, Some(KillContainerOptionsBuilder::new().signal("KILL").build()))
                        .await;
                    flush_lines(log, [&mut stdout, &mut stderr]);
                    log.line("canceled");
                    return Err(JobError::Canceled);
                }
            }
        }
        flush_lines(log, [&mut stdout, &mut stderr]);
        let mut wait = self.docker.wait_container(
            id,
            Some(
                WaitContainerOptionsBuilder::new()
                    .condition("not-running")
                    .build(),
            ),
        );
        match wait.next().await {
            Some(Ok(r)) => Ok(i32::try_from(r.status_code).unwrap_or(-1)),
            // bollard reports a non-zero exit status as an error.
            Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                Ok(i32::try_from(code).unwrap_or(-1))
            }
            Some(Err(e)) => Err(errored("waiting for a container", e)),
            None => Err(JobError::Errored("the container wait ended early".into())),
        }
    }

    async fn remove_container(&self, id: &str) {
        if let Err(e) = self
            .docker
            .remove_container(
                id,
                Some(RemoveContainerOptionsBuilder::new().force(true).build()),
            )
            .await
        {
            tracing::warn!(error = %e, container = id, "removing a container failed");
        }
    }

    async fn cleanup(&self, res: Resources) {
        for id in &res.containers {
            self.remove_container(id).await;
        }
        if let Some(volume) = &res.volume
            && let Err(e) = self
                .docker
                .remove_volume(
                    volume,
                    Some(RemoveVolumeOptionsBuilder::new().force(true).build()),
                )
                .await
        {
            tracing::warn!(error = %e, volume, "removing a volume failed");
        }
    }
}

/// Labels of everything created for a job: the executor and the job.
fn job_labels(job: &Job, ctx: &JobContext) -> HashMap<String, String> {
    [
        (LABEL.to_owned(), ctx.executor.clone()),
        (format!("{LABEL}.job"), format!("{}-{}", job.queue, job.id)),
    ]
    .into()
}

fn volume_mount(volume: &str) -> Mount {
    Mount {
        target: Some(DATA.into()),
        source: Some(volume.to_owned()),
        typ: Some(MountTypeEnum::VOLUME),
        ..Default::default()
    }
}

/// Git configuration of every step container: the workspace is copied into the volume as the
/// executor's user, so git in a step running as another user (most indexer images run as root)
/// would refuse the repository as having dubious ownership, and indexers that ask git for the
/// commit fail. A step or secret that sets `GIT_CONFIG_*` itself overrides this.
const STEP_GIT_ENV: [&str; 3] = [
    "GIT_CONFIG_COUNT=1",
    "GIT_CONFIG_KEY_0=safe.directory",
    "GIT_CONFIG_VALUE_0=*",
];

/// The environment of a step container: [`STEP_GIT_ENV`], the step's own entries, then the
/// executor secrets.
pub fn step_env(job: &Job, step: &DockerStep) -> Vec<String> {
    let mut env: Vec<String> = STEP_GIT_ENV.iter().map(|&e| e.to_owned()).collect();
    env.extend(step.env.iter().cloned());
    env.extend(job.secret_env.iter().cloned());
    env
}

/// The registry an image is pulled from, as Docker names it: the first path segment when it
/// looks like a host (has a `.` or `:`, or is `localhost`), else `docker.io`.
pub fn image_registry(image: &str) -> &str {
    match image.split_once('/') {
        Some((host, _)) if host.contains(['.', ':']) || host == "localhost" => host,
        _ => "docker.io",
    }
}

/// A registry as written in a Docker config's `auths` (`https://index.docker.io/v1/`,
/// `registry.example.com`), reduced to its host; Docker Hub's names become `docker.io`.
fn normalize_registry(key: &str) -> &str {
    let host = key
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    match host {
        "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => "docker.io",
        h => h,
    }
}

/// The credentials of `config` for pulling `image`, if it has some for its registry.
pub fn registry_credentials(config: &DockerAuthConfig, image: &str) -> Option<DockerCredentials> {
    let registry = image_registry(image);
    let (key, auth) = config
        .auths
        .iter()
        .find(|(key, _)| normalize_registry(key) == registry)?;
    let (username, password) = decode_auth(&auth.auth)?;
    Some(DockerCredentials {
        username: Some(username),
        password: Some(password),
        serveraddress: Some(key.clone()),
        ..Default::default()
    })
}

/// Packs the contents of `dir` into a tar file (entries relative to `dir`).
fn pack(dir: &Path, tarball: &Path) -> std::io::Result<()> {
    let file = std::fs::File::create(tarball)?;
    let mut builder = tar::Builder::new(std::io::BufWriter::new(file));
    builder.follow_symlinks(false);
    builder.append_dir_all(".", dir)?;
    builder
        .into_inner()?
        .into_inner()
        .map_err(|e| e.into_error())?;
    Ok(())
}

/// Extracts the single regular file of a tar archive to `dest`.
fn unpack_single(tarball: &Path, dest: &Path) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(std::fs::File::open(tarball)?);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() {
            let mut out = std::fs::File::create(dest)?;
            std::io::copy(&mut entry, &mut out)?;
            let _ = std::fs::remove_file(tarball);
            return Ok(());
        }
    }
    Err(std::io::Error::other("the path is not a regular file"))
}

/// Logs the unfinished last lines of `buffers`.
fn flush_lines<const N: usize>(log: &StepLog, buffers: [&mut LineBuffer; N]) {
    for buf in buffers {
        if let Some(text) = buf.finish() {
            log.append(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images() {
        assert_eq!(split_image("alpine"), ("alpine", "latest"));
        assert_eq!(split_image("alpine@sha256:abc"), ("alpine", "sha256:abc"));
        assert_eq!(
            split_image("sourcegraph/scip-go:v0.1"),
            ("sourcegraph/scip-go", "v0.1")
        );
        assert_eq!(
            split_image("registry:5000/team/img"),
            ("registry:5000/team/img", "latest")
        );
        assert_eq!(
            split_image("registry:5000/team/img:1.2"),
            ("registry:5000/team/img", "1.2")
        );
    }

    #[test]
    fn registries_and_credentials() {
        assert_eq!(image_registry("alpine"), "docker.io");
        assert_eq!(image_registry("sourcegraph/scip-go"), "docker.io");
        assert_eq!(
            image_registry("registry.example.com/team/img:1"),
            "registry.example.com"
        );
        assert_eq!(image_registry("localhost:5000/img"), "localhost:5000");
        let config = DockerAuthConfig {
            auths: [
                (
                    "https://index.docker.io/v1/".to_owned(),
                    sourcegull_executor_api::DockerAuthConfigAuth {
                        auth: "aHViOmh1Yi1wYXNz".into(), // hub:hub-pass
                    },
                ),
                (
                    "localhost:5000".to_owned(),
                    sourcegull_executor_api::DockerAuthConfigAuth {
                        auth: "dXNlcjpwYXNz".into(), // user:pass
                    },
                ),
            ]
            .into(),
        };
        let hub = registry_credentials(&config, "sourcegraph/scip-go").unwrap();
        assert_eq!(hub.username.as_deref(), Some("hub"));
        assert_eq!(hub.password.as_deref(), Some("hub-pass"));
        let local = registry_credentials(&config, "localhost:5000/img:1").unwrap();
        assert_eq!(local.serveraddress.as_deref(), Some("localhost:5000"));
        assert!(registry_credentials(&config, "quay.io/x/y").is_none());
        let job = Job {
            secret_env: vec!["TOKEN=s3cret".into()],
            ..Default::default()
        };
        let step = DockerStep {
            env: vec!["A=1".into()],
            ..Default::default()
        };
        assert_eq!(
            step_env(&job, &step),
            [
                "GIT_CONFIG_COUNT=1",
                "GIT_CONFIG_KEY_0=safe.directory",
                "GIT_CONFIG_VALUE_0=*",
                "A=1",
                "TOKEN=s3cret"
            ]
        );
    }

    #[test]
    fn workdirs_and_paths() {
        let step = DockerStep {
            dir: "web/./app/".into(),
            ..Default::default()
        };
        assert_eq!(step_workdir(&step).unwrap(), "/data/repository/web/app");
        let escape = DockerStep {
            dir: "../etc".into(),
            ..Default::default()
        };
        assert!(step_workdir(&escape).is_err());
        assert_eq!(
            repo_path("web/index.scip").unwrap(),
            "/data/repository/web/index.scip"
        );
        assert!(repo_path("../x").is_err());
        assert!(repo_path("").is_err());
    }

    #[test]
    fn scripts_stop_at_the_first_failure() {
        let step = DockerStep {
            commands: vec!["npm ci".into(), "scip-typescript index".into()],
            ..Default::default()
        };
        assert_eq!(
            step_script(&step),
            "set -e\nnpm ci\nscip-typescript index\n"
        );
    }
}
