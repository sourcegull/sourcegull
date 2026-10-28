//! The Docker [`ContainerRuntime`] (through `bollard`): one container per step.
//!
//! Before a step's container starts, one archive is uploaded into it (`PUT /archive` on the
//! created container): the script at [`SCRIPT_PATH`], the step's `files` at their paths, its
//! `mount`s copied to their mount points, and — in [`WorkspaceMode::Copy`] — the working copy
//! at [`WORKSPACE_DIR`]. The container runs `/bin/sh` on the script in the step's working
//! directory, output is streamed, and in copy mode [`WORKSPACE_DIR`] is copied back over the
//! host working copy after it exits. The container is always removed.
//!
//! - [`WorkspaceMode::Bind`] bind-mounts the host working copy at [`WORKSPACE_DIR`]: no
//!   copying, but the daemon must see the same path as the caller (`gull bulk` on a
//!   developer machine). Files the container creates belong to its user. The working tree
//!   (not `.git`) is made writable for everyone before each step, so an image running as
//!   another user can change it.
//! - [`WorkspaceMode::Copy`] works wherever the daemon is (an executor in a container with
//!   the Docker socket mounted, a remote daemon), at the cost of two copies per step.
//!   Everything copied in belongs to the user the image runs as
//!   ([`DockerRuntime::image_user`]), so non-root images can write the working copy and `git`
//!   in a step does not find it owned by someone else.
//!
//! Images are pulled when missing (or always, with [`DockerOptions::always_pull`]), with
//! credentials from [`DockerRuntime::with_credentials`], and run by image ID, which is the
//! digest the runner caches by.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DownloadFromContainerOptionsBuilder,
    KillContainerOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    UploadToContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::runtime::{
    ContainerRuntime, ContainerSpec, OutputFn, OutputStream, RuntimeError, SHELL, WORKSPACE_DIR,
};

/// Where the step's script is put in its container.
pub const SCRIPT_PATH: &str = "/tmp/.sourcegull-step.sh";
/// Label on every container the runtime creates.
pub const LABEL: &str = "io.sourcegull.batches-step";

/// How the working copy gets into step containers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceMode {
    /// Bind-mount the host directory.
    #[default]
    Bind,
    /// Copy it in before and out after each step.
    Copy,
}

/// Settings of the Docker runtime.
#[derive(Debug, Clone)]
pub struct DockerOptions {
    pub workspace_mode: WorkspaceMode,
    /// Network of step containers: `bridge` (default), `none` or a named network.
    pub network: String,
    /// Memory limit per step container in bytes (0: none).
    pub memory_bytes: i64,
    /// CPU limit per step container (0: none).
    pub cpus: f64,
    /// Pull images that are already present (to pick up moved tags).
    pub always_pull: bool,
    /// Extra labels on step containers (for example the executor and job).
    pub labels: HashMap<String, String>,
}

impl Default for DockerOptions {
    fn default() -> Self {
        Self {
            workspace_mode: WorkspaceMode::default(),
            network: "bridge".into(),
            memory_bytes: 0,
            cpus: 0.0,
            always_pull: false,
            labels: HashMap::new(),
        }
    }
}

/// Credentials for pulling an image, by image name.
pub type CredentialsFn = Arc<dyn Fn(&str) -> Option<DockerCredentials> + Send + Sync>;

/// The numeric user and group a container runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageUser {
    pub uid: u32,
    pub gid: u32,
}

impl ImageUser {
    pub const ROOT: ImageUser = ImageUser { uid: 0, gid: 0 };

    /// The user of an image's `User` setting when it is known without running the image:
    /// none set (root) or numeric `uid:gid`. A name, or a uid without a group, needs a look
    /// at the image's `/etc/passwd` and `/etc/group` ([`ID_SCRIPT`]).
    pub fn from_config(user: &str) -> Option<ImageUser> {
        if user.is_empty() {
            return Some(ImageUser::ROOT);
        }
        let (uid, gid) = user.split_once(':')?;
        Some(ImageUser {
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
        })
    }

    /// Parses the output of [`ID_SCRIPT`].
    pub fn from_id_output(out: &str) -> Option<ImageUser> {
        let mut lines = out.split_whitespace();
        let user = ImageUser {
            uid: lines.next()?.parse().ok()?,
            gid: lines.next()?.parse().ok()?,
        };
        lines.next().is_none().then_some(user)
    }
}

/// What runs in an image to find the user and group it runs as.
pub const ID_SCRIPT: &str = "id -u; id -g";

/// Runs steps with a Docker daemon.
pub struct DockerRuntime {
    docker: Docker,
    options: DockerOptions,
    credentials: Option<CredentialsFn>,
    scratch: PathBuf,
    /// [`DockerRuntime::image_user`] by image ID; `None` when it could not be determined.
    users: Mutex<HashMap<String, Option<ImageUser>>>,
}

fn other(what: &str, e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Other(format!("{what}: {e}"))
}

impl DockerRuntime {
    /// Connects with the usual defaults (`DOCKER_HOST`, else the local socket).
    pub fn connect(options: DockerOptions) -> Result<Self, RuntimeError> {
        let docker =
            Docker::connect_with_defaults().map_err(|e| other("connecting to Docker", e))?;
        Ok(Self::new(docker, options))
    }

    pub fn new(docker: Docker, options: DockerOptions) -> Self {
        Self {
            docker,
            options,
            credentials: None,
            scratch: std::env::temp_dir(),
            users: Mutex::new(HashMap::new()),
        }
    }

    /// Credentials for image pulls (the executor's `DOCKER_AUTH_CONFIG` by registry).
    pub fn with_credentials(mut self, credentials: CredentialsFn) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Where archives are staged while copying (default: the system temp directory).
    pub fn with_scratch_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.scratch = dir.into();
        self
    }

    /// The daemon's version; empty when it is not reachable.
    pub async fn version(&self) -> String {
        self.docker
            .version()
            .await
            .ok()
            .and_then(|v| v.version)
            .unwrap_or_default()
    }

    async fn pull(&self, image: &str, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        let (name, tag) = split_image(image);
        let credentials = self.credentials.as_ref().and_then(|f| f(image));
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
                _ = cancel.cancelled() => return Err(RuntimeError::Canceled),
            };
            match item {
                None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    return Err(RuntimeError::Image {
                        image: image.to_owned(),
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    /// The user and group containers of the image `digest` run as: from
    /// the image's `User` when that is empty or numeric, else by running [`ID_SCRIPT`] in it.
    /// Memoized; `None` (with a warning) when the image cannot tell, for example without `id`.
    pub async fn image_user(&self, digest: &str, cancel: &CancellationToken) -> Option<ImageUser> {
        if let Some(u) = self
            .users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(digest)
        {
            return *u;
        }
        let configured = match self.docker.inspect_image(digest).await {
            Ok(i) => i.config.and_then(|c| c.user).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, image = digest, "inspecting the image failed");
                return None;
            }
        };
        let user = match ImageUser::from_config(&configured) {
            Some(u) => Some(u),
            None => match self.run_id(digest, cancel).await {
                Ok(u) => Some(u),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        image = digest,
                        "could not determine the image's user; the working copy keeps its owner"
                    );
                    None
                }
            },
        };
        self.users
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(digest.to_owned(), user);
        user
    }

    /// Runs [`ID_SCRIPT`] in the image.
    async fn run_id(
        &self,
        digest: &str,
        cancel: &CancellationToken,
    ) -> Result<ImageUser, RuntimeError> {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("sourcegull-step-id-{}", &suffix[..12]);
        let mut labels = self.options.labels.clone();
        labels.insert(LABEL.to_owned(), "id".to_owned());
        let body = ContainerCreateBody {
            image: Some(digest.to_owned()),
            entrypoint: Some(vec![SHELL.into()]),
            cmd: Some(vec!["-c".into(), ID_SCRIPT.into()]),
            labels: Some(labels),
            host_config: Some(HostConfig {
                network_mode: Some("none".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = async {
            self.docker
                .create_container(
                    Some(CreateContainerOptionsBuilder::new().name(&name).build()),
                    body,
                )
                .await
                .map_err(|e| other("creating the container", e))?;
            let out = Mutex::new(Vec::new());
            let sink = |stream: OutputStream, data: &[u8]| {
                if stream == OutputStream::Stdout {
                    out.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(data);
                }
            };
            let code = self.run_container(&name, &sink, cancel).await?;
            let out = out.into_inner().unwrap_or_else(|e| e.into_inner());
            let out = String::from_utf8_lossy(&out);
            match ImageUser::from_id_output(&out) {
                Some(u) if code == 0 => Ok(u),
                _ => Err(RuntimeError::Other(format!(
                    "`{ID_SCRIPT}` exited with status {code}: {:?}",
                    out.trim()
                ))),
            }
        }
        .await;
        self.remove_container(&name).await;
        result
    }

    async fn run_inner(
        &self,
        spec: &ContainerSpec,
        name: &str,
        archive: &Path,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        let workspace = spec.workspace.clone();
        let copy = self.options.workspace_mode == WorkspaceMode::Copy;
        let owner = if copy {
            self.image_user(&spec.digest, cancel).await
        } else {
            let workspace = workspace.clone();
            tokio::task::spawn_blocking(move || make_writable(&workspace))
                .await
                .map_err(|e| other("making the working copy writable", e))?;
            None
        };
        {
            let (spec, archive) = (spec.clone(), archive.to_owned());
            tokio::task::spawn_blocking(move || pack_step(&spec, copy, owner, &archive))
                .await
                .map_err(|e| other("packing the step's files", e))?
                .map_err(|e| other("packing the step's files", e))?;
        }

        let mut labels = self.options.labels.clone();
        labels.insert(LABEL.to_owned(), spec.step.to_string());
        let binds = if copy {
            None
        } else {
            let abs = tokio::fs::canonicalize(&workspace)
                .await
                .map_err(|e| other("resolving the workspace path", e))?;
            Some(vec![format!("{}:{WORKSPACE_DIR}", abs.display())])
        };
        let body = ContainerCreateBody {
            image: Some(spec.digest.clone()),
            entrypoint: Some(vec![SHELL.into()]),
            cmd: Some(vec![SCRIPT_PATH.into()]),
            working_dir: Some(spec.workdir.clone()),
            env: Some(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            labels: Some(labels),
            host_config: Some(HostConfig {
                binds,
                network_mode: Some(self.options.network.clone()),
                memory: (self.options.memory_bytes > 0).then_some(self.options.memory_bytes),
                nano_cpus: (self.options.cpus > 0.0).then_some((self.options.cpus * 1e9) as i64),
                init: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.docker
            .create_container(
                Some(CreateContainerOptionsBuilder::new().name(name).build()),
                body,
            )
            .await
            .map_err(|e| other("creating the step container", e))?;

        let file = tokio::fs::File::open(archive)
            .await
            .map_err(|e| other("reading the step archive", e))?;
        self.docker
            .upload_to_container(
                name,
                Some(UploadToContainerOptionsBuilder::new().path("/").build()),
                bollard::body_try_stream(tokio_util::io::ReaderStream::new(file)),
            )
            .await
            .map_err(|e| other("copying files into the step container", e))?;
        let _ = tokio::fs::remove_file(archive).await;

        let code = self.run_container(name, output, cancel).await?;

        if copy {
            self.copy_back(name, &workspace, archive).await?;
        }
        Ok(code)
    }

    /// Starts a container, streams its output and returns its exit code. Killed on
    /// cancellation.
    async fn run_container(
        &self,
        id: &str,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        self.docker
            .start_container(id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .map_err(|e| other("starting the step container", e))?;
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
        loop {
            tokio::select! {
                item = logs.next() => match item {
                    Some(Ok(LogOutput::StdOut { message })) => output(OutputStream::Stdout, &message),
                    Some(Ok(LogOutput::StdErr { message })) => output(OutputStream::Stderr, &message),
                    Some(Ok(LogOutput::Console { message })) => output(OutputStream::Stdout, &message),
                    Some(Ok(LogOutput::StdIn { .. })) => {}
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
                    return Err(RuntimeError::Canceled);
                }
            }
        }
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
            Some(Err(e)) => Err(other("waiting for the step container", e)),
            None => Err(RuntimeError::Other("the container wait ended early".into())),
        }
    }

    /// Replaces the host working copy with the container's [`WORKSPACE_DIR`].
    async fn copy_back(
        &self,
        id: &str,
        workspace: &Path,
        archive: &Path,
    ) -> Result<(), RuntimeError> {
        {
            let mut stream = self.docker.download_from_container(
                id,
                Some(
                    DownloadFromContainerOptionsBuilder::new()
                        .path(WORKSPACE_DIR)
                        .build(),
                ),
            );
            let mut f = tokio::fs::File::create(archive)
                .await
                .map_err(|e| other("copying the workspace back", e))?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| other("copying the workspace back", e))?;
                f.write_all(&chunk)
                    .await
                    .map_err(|e| other("copying the workspace back", e))?;
            }
            f.flush()
                .await
                .map_err(|e| other("copying the workspace back", e))?;
        }
        let (workspace, archive) = (workspace.to_owned(), archive.to_owned());
        tokio::task::spawn_blocking(move || {
            let r = unpack_workspace(&archive, &workspace);
            let _ = std::fs::remove_file(&archive);
            r
        })
        .await
        .map_err(|e| other("copying the workspace back", e))?
        .map_err(|e| other("copying the workspace back", e))
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
            tracing::debug!(error = %e, container = id, "removing a step container failed");
        }
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn resolve_image(
        &self,
        image: &str,
        cancel: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        if self.options.always_pull || self.docker.inspect_image(image).await.is_err() {
            self.pull(image, cancel).await?;
        }
        let inspect = self
            .docker
            .inspect_image(image)
            .await
            .map_err(|e| RuntimeError::Image {
                image: image.to_owned(),
                message: e.to_string(),
            })?;
        inspect.id.ok_or_else(|| RuntimeError::Image {
            image: image.to_owned(),
            message: "the image has no ID".into(),
        })
    }

    async fn run(
        &self,
        spec: &ContainerSpec,
        output: OutputFn<'_>,
        cancel: &CancellationToken,
    ) -> Result<i32, RuntimeError> {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let name = format!("sourcegull-step-{}-{}", spec.step, &suffix[..12]);
        let archive = self.scratch.join(format!("{name}.tar"));
        let result = self.run_inner(spec, &name, &archive, output, cancel).await;
        self.remove_container(&name).await;
        let _ = tokio::fs::remove_file(&archive).await;
        result
    }
}

/// `name[:tag]` (or `name@digest`) split for the pull API; the tag defaults to `latest`.
pub fn split_image(image: &str) -> (&str, &str) {
    if let Some((name, digest)) = image.split_once('@') {
        // The pull API takes a digest in place of the tag.
        return (name, digest);
    }
    match image.rsplit_once(':') {
        // A colon in the last path segment is a tag; otherwise it is a registry port.
        Some((name, tag)) if !tag.contains('/') => (name, tag),
        _ => (image, "latest"),
    }
}

/// The archive uploaded to `/` of a step container; with `owner`, every entry belongs to it.
fn pack_step(
    spec: &ContainerSpec,
    copy_workspace: bool,
    owner: Option<ImageUser>,
    archive: &Path,
) -> std::io::Result<()> {
    let file = std::fs::File::create(archive)?;
    let mut b = tar::Builder::new(std::io::BufWriter::new(file));
    append_file_owned(&mut b, SCRIPT_PATH, spec.script.as_bytes(), 0o755, owner)?;
    for f in &spec.files {
        append_file_owned(&mut b, &f.target, &f.content, 0o644, owner)?;
    }
    for m in &spec.mounts {
        append_tree(
            &mut b,
            Path::new(m.target.trim_start_matches('/')),
            &m.source,
            owner,
        )?;
    }
    if copy_workspace {
        append_tree(
            &mut b,
            Path::new(WORKSPACE_DIR.trim_start_matches('/')),
            &spec.workspace,
            owner,
        )?;
    }
    b.into_inner()?.into_inner().map_err(|e| e.into_error())?;
    Ok(())
}

/// Appends `source` (a file, a symlink, or a directory with everything in it; symlinks are
/// not followed) at `name`, with its modes and times and, with `owner`, that owner. Other
/// file types are left out. Also used by the executor's Kubernetes runtime.
pub fn append_tree<W: std::io::Write>(
    b: &mut tar::Builder<W>,
    name: &Path,
    source: &Path,
    owner: Option<ImageUser>,
) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    let mut h = tar::Header::new_gnu();
    h.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
    if let Some(o) = owner {
        h.set_uid(u64::from(o.uid));
        h.set_gid(u64::from(o.gid));
    }
    let kind = meta.file_type();
    if kind.is_dir() {
        h.set_size(0);
        b.append_data(&mut h, name, std::io::empty())?;
        let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            append_tree(b, &name.join(e.file_name()), &e.path(), owner)?;
        }
    } else if kind.is_symlink() {
        h.set_size(0);
        b.append_link(&mut h, name, std::fs::read_link(source)?)?;
    } else if kind.is_file() {
        b.append_data(&mut h, name, std::fs::File::open(source)?)?;
    }
    Ok(())
}

/// Makes the working tree under `workspace` (not `.git`) writable for everyone: directories
/// `rwx` for all, files `rw` (and `x` if executable by
/// anyone). Entries already so, and ones this user may not change (created by a container
/// running as someone else), are left alone.
pub fn make_writable(workspace: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fn walk(dir: &Path, top: bool) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            if top && e.file_name() == ".git" {
                continue;
            }
            let path = e.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let mode = meta.permissions().mode() & 0o7777;
            let want = if meta.is_dir() || (meta.is_file() && mode & 0o111 != 0) {
                mode | 0o777
            } else if meta.is_file() {
                mode | 0o666
            } else {
                continue;
            };
            if want != mode {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(want));
            }
            if meta.is_dir() {
                walk(&path, false);
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(workspace) {
        let mode = meta.permissions().mode() & 0o7777;
        if mode | 0o777 != mode {
            let _ =
                std::fs::set_permissions(workspace, std::fs::Permissions::from_mode(mode | 0o777));
        }
    }
    walk(workspace, true);
}

/// Appends a regular file with `content` at `path` (a leading `/` is dropped).
pub fn append_file<W: std::io::Write>(
    b: &mut tar::Builder<W>,
    path: &str,
    content: &[u8],
    mode: u32,
) -> std::io::Result<()> {
    append_file_owned(b, path, content, mode, None)
}

/// [`append_file`] with an owner (root without one).
pub fn append_file_owned<W: std::io::Write>(
    b: &mut tar::Builder<W>,
    path: &str,
    content: &[u8],
    mode: u32,
    owner: Option<ImageUser>,
) -> std::io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(content.len() as u64);
    h.set_mode(mode);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mtime(0);
    let o = owner.unwrap_or(ImageUser::ROOT);
    h.set_uid(u64::from(o.uid));
    h.set_gid(u64::from(o.gid));
    b.append_data(&mut h, path.trim_start_matches('/'), content)
}

/// Empties `workspace` and extracts the archive of [`WORKSPACE_DIR`] into it (the archive's
/// entries start with `work/`). Entries that would land outside the workspace (`..`, through
/// a symlink) are refused; hard links are extracted as copies. Also used by the executor's
/// Kubernetes runtime.
pub fn unpack_workspace(archive: &Path, workspace: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(workspace)? {
        let path = entry?.path();
        if std::fs::symlink_metadata(&path)?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    let prefix = WORKSPACE_DIR.trim_start_matches('/');
    let inside = |path: &Path| -> Option<PathBuf> {
        let rel = path.strip_prefix(prefix).ok()?;
        let normal = rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        (normal && !rel.as_os_str().is_empty()).then(|| rel.to_owned())
    };
    let mut tar = tar::Archive::new(std::fs::File::open(archive)?);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.as_os_str() == prefix || path.as_os_str() == "work/" {
            continue;
        }
        let refuse = || std::io::Error::other(format!("refusing to extract {}", path.display()));
        let rel = inside(&path).ok_or_else(refuse)?;
        let dest = workspace.join(&rel);
        // No parent may be a symlink, or the entry could be written outside the workspace.
        let mut parent = workspace.to_owned();
        for c in rel.parent().into_iter().flat_map(|p| p.components()) {
            parent.push(c);
            match std::fs::symlink_metadata(&parent) {
                Ok(m) if m.file_type().is_symlink() => return Err(refuse()),
                Ok(_) => {}
                Err(_) => std::fs::create_dir_all(&parent)?,
            }
        }
        if entry.header().entry_type().is_hard_link() {
            let target = entry
                .link_name()?
                .and_then(|l| inside(&l))
                .ok_or_else(refuse)?;
            std::fs::copy(workspace.join(target), &dest)?;
            continue;
        }
        entry.unpack(&dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_users() {
        assert_eq!(ImageUser::from_config(""), Some(ImageUser::ROOT));
        assert_eq!(
            ImageUser::from_config("1000:1001"),
            Some(ImageUser {
                uid: 1000,
                gid: 1001
            })
        );
        // Names and a uid alone need the image's passwd file.
        assert_eq!(ImageUser::from_config("node"), None);
        assert_eq!(ImageUser::from_config("1000"), None);
        assert_eq!(ImageUser::from_config("1000:staff"), None);
        assert_eq!(
            ImageUser::from_id_output("1234\n1234\n"),
            Some(ImageUser {
                uid: 1234,
                gid: 1234
            })
        );
        assert_eq!(ImageUser::from_id_output("1234\n"), None);
        assert_eq!(ImageUser::from_id_output("sh: id: not found\n"), None);
    }

    #[test]
    fn packs_owned_by_the_image_user() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/a.sh"), "x").unwrap();
        std::fs::set_permissions(ws.join("sub/a.sh"), std::fs::Permissions::from_mode(0o750))
            .unwrap();
        std::os::unix::fs::symlink("sub/a.sh", ws.join("link")).unwrap();
        let spec = ContainerSpec {
            step: 0,
            image: "alpine".into(),
            digest: "sha256:x".into(),
            workspace: ws.clone(),
            workdir: WORKSPACE_DIR.into(),
            script: "echo".into(),
            env: Default::default(),
            files: vec![],
            mounts: vec![],
        };
        let archive = tmp.path().join("a.tar");
        let owner = ImageUser { uid: 1234, gid: 99 };
        pack_step(&spec, true, Some(owner), &archive).unwrap();
        let mut tar = tar::Archive::new(std::fs::File::open(&archive).unwrap());
        let mut seen = Vec::new();
        for e in tar.entries().unwrap() {
            let e = e.unwrap();
            let h = e.header();
            assert_eq!((h.uid().unwrap(), h.gid().unwrap()), (1234, 99));
            let path = e.path().unwrap().to_string_lossy().into_owned();
            if path == "work/sub/a.sh" {
                assert_eq!(h.mode().unwrap() & 0o777, 0o750);
            }
            if path == "work/link" {
                assert_eq!(e.link_name().unwrap().unwrap().to_str(), Some("sub/a.sh"));
            }
            seen.push(path);
        }
        assert_eq!(
            seen,
            [
                "tmp/.sourcegull-step.sh",
                "work",
                "work/link",
                "work/sub",
                "work/sub/a.sh"
            ]
        );
    }

    #[test]
    fn makes_the_working_tree_writable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(ws.join("src/a.txt"), "a").unwrap();
        std::fs::write(ws.join("run.sh"), "a").unwrap();
        let mode = |p: &str| std::fs::metadata(ws.join(p)).unwrap().permissions().mode() & 0o777;
        let set = |p: &str, m: u32| {
            std::fs::set_permissions(ws.join(p), std::fs::Permissions::from_mode(m)).unwrap()
        };
        set("src", 0o755);
        set("src/a.txt", 0o644);
        set("run.sh", 0o744);
        set(".git", 0o755);
        make_writable(ws);
        assert_eq!(mode("src"), 0o777);
        assert_eq!(mode("src/a.txt"), 0o666);
        assert_eq!(mode("run.sh"), 0o777);
        assert_eq!(mode(".git"), 0o755);
    }

    #[test]
    fn images() {
        assert_eq!(split_image("alpine"), ("alpine", "latest"));
        assert_eq!(split_image("alpine@sha256:abc"), ("alpine", "sha256:abc"));
        assert_eq!(
            split_image("registry:5000/team/img"),
            ("registry:5000/team/img", "latest")
        );
        assert_eq!(
            split_image("registry:5000/team/img:1.2"),
            ("registry:5000/team/img", "1.2")
        );
    }
}
