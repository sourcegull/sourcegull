//! The container runtime the steps run in.
//!
//! A [`ContainerRuntime`] runs one step: a container from an image (pinned by digest), with the
//! host working copy visible at [`WORKSPACE_DIR`], the step's script run by `/bin/sh` in
//! [`ContainerSpec::workdir`], the step's environment, its `files` written at their paths and
//! its `mount`s copied or bound at their mount points. It streams output while the container
//! runs and returns the exit code. Changes the step makes under [`WORKSPACE_DIR`] must be in
//! the host working copy when [`ContainerRuntime::run`] returns; the runner computes the diff
//! there with `git`.
//!
//! Implementations: [`crate::docker::DockerRuntime`] and the in-process
//! [`crate::fake::FakeRuntime`] for tests; the executor's Kubernetes runtime
//! (`sourcegull_executor::kubernetes::steps`) implements the same trait.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Where the working copy (the repository root) is in step containers.
pub const WORKSPACE_DIR: &str = "/work";

/// The shell that runs step scripts: the image's `/bin/sh`.
pub const SHELL: &str = "/bin/sh";

/// Which output stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// A file to put into the container before the step starts (a step's `files` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerFile {
    /// Absolute path in the container.
    pub target: String,
    pub content: Vec<u8>,
}

/// A host file or directory to make visible in the container (a step's `mount` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMount {
    /// Host path (resolved and checked by the runner).
    pub source: PathBuf,
    /// Absolute path in the container.
    pub target: String,
}

/// Everything a runtime needs to run one step.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    /// The step's index (from 0), for container names and logs.
    pub step: usize,
    /// The image as written in the spec (for logs and pulls).
    pub image: String,
    /// The image digest from [`ContainerRuntime::resolve_image`]; what is run, so a tag moving
    /// during a run cannot change what runs.
    pub digest: String,
    /// The host working copy, made visible at [`WORKSPACE_DIR`].
    pub workspace: PathBuf,
    /// The working directory in the container: [`WORKSPACE_DIR`] plus the workspace path.
    pub workdir: String,
    /// The rendered `run`, executed with [`SHELL`] as a script file (not `sh -c`).
    pub script: String,
    /// The final environment (rendered, pass-through values filled in).
    pub env: BTreeMap<String, String>,
    pub files: Vec<ContainerFile>,
    pub mounts: Vec<HostMount>,
}

/// Receives output while a container runs.
pub type OutputFn<'a> = &'a (dyn Fn(OutputStream, &[u8]) + Send + Sync);

/// Runtime errors (not a step's non-zero exit, which is an exit code).
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The image does not exist or cannot be pulled.
    #[error("image {image}: {message}")]
    Image { image: String, message: String },
    #[error("canceled")]
    Canceled,
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Makes `image` available (pulling it when missing) and returns its digest, an immutable
    /// reference to it that [`ContainerSpec::digest`] is set to and step cache keys contain.
    async fn resolve_image(
        &self,
        image: &str,
        cancel: &CancellationToken,
    ) -> Result<String, RuntimeError>;

    /// Runs one step to completion and returns its exit code. Output is passed to `output` as
    /// it arrives; stdout and stderr are kept apart. On cancellation the container is killed
    /// and [`RuntimeError::Canceled`] returned.
    async fn run(
        &self,
        spec: &ContainerSpec,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError>;
}
