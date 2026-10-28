//! The batch step runner: runs a bulk change spec's steps in one workspace, caches step results and
//! builds the workspace's changeset specs. Used by the server-side executor (the `batches`
//! queue, Docker and Kubernetes) and by `gull bulk preview|apply` (Docker).
//!
//! # Running a workspace
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use sourcegull_batches_steps::*;
//! # async fn example(spec: sourcegull_batches::BatchSpec, workspace: &std::path::Path) -> Result<(), StepsError> {
//! let runtime = Arc::new(docker::DockerRuntime::connect(docker::DockerOptions::default())
//!     .map_err(StepsError::Image)?);
//! let cache = Arc::new(MemoryStepCache::new());
//! let runner = StepRunner::new(runtime)
//!     .with_cache(cache)
//!     .with_log(Arc::new(|e: &StepEvent<'_>| println!("{e:?}")));
//! let task = Task {
//!     spec: spec.clone(),
//!     repository: TaskRepository {
//!         name: "gitlab.example.com/group/project".into(),
//!         branch: "main".into(),
//!         base_rev: "0123abcd…".into(),
//!         file_matches: vec!["go.mod".into()],
//!     },
//!     path: String::new(),
//!     only_fetch_workspace: false,
//!     global_env: Default::default(),
//! };
//! // `workspace`: a clean checkout of the repository at `base_rev`.
//! let outcome = runner.run(&task, workspace).await?;
//! let repo = SpecRepository {
//!     id: "UmVwb3NpdG9yeTox".into(),
//!     name: task.repository.name.clone(),
//!     base_ref: "refs/heads/main".into(),
//!     base_rev: task.repository.base_rev.clone(),
//!     file_matches: task.repository.file_matches.clone(),
//! };
//! let specs = build_changeset_specs(&ChangesetSpecInput {
//!     spec: &spec,
//!     repository: &repo,
//!     path: &task.path,
//!     result: &outcome.result,
//!     fallback_author: None,
//! })?;
//! for s in specs {
//!     println!("{}", s.to_json()); // what `createChangesetSpec` accepts
//! }
//! # Ok(()) }
//! ```
//!
//! [`StepRunner::run`] does, for one workspace:
//!
//! 1. Resolves every step's image to a digest ([`ContainerRuntime::resolve_image`], pulling
//!    when missing); steps run by digest.
//! 2. With a [`StepCache`]: computes each step's [`StepCacheKey`] ([`step_cache_keys`]: the
//!    resolver's key plus a fingerprint of image digests, env values after pass-through and
//!    mounted file contents, see [`cache`]), finds the last step with a matching entry, applies
//!    its diff to the working copy and resumes after it (or returns it if it was the last).
//! 3. For each remaining step, with a `StepContext` (`repository`, `batch_change`,
//!    `outputs` so far, `steps.path`, `steps.*_files` and `previous_step` from the previous
//!    step that ran, `step` empty): evaluates `if:` (skipped steps leave no result and do not
//!    change `previous_step`); renders `run`, `env` (after filling pass-through values from
//!    [`Task::global_env`]) and `files`; resolves `mount` paths under the mount root; runs the
//!    container ([`ContainerRuntime::run`]), streaming output to the [`LogSink`]; fails on a
//!    non-zero exit; computes changed files (`git add --all; git status --porcelain`); sets
//!    `step` to the step's result and evaluates `outputs` in name order (`text`, `json`,
//!    `yaml`); computes the diff (`git diff --cached --no-prefix --binary`, cumulative from the
//!    base commit); stores the [`AfterStepResult`] in the cache.
//! 4. Returns a [`RunOutcome`] whose `result` is the final state: diff, changed files, outputs.
//!
//! [`build_changeset_specs`] turns a final result into changeset specs ([`changeset`]); it
//! needs no runtime, so a fully cached workspace (`cachedResultFound`) is built from its cached
//! [`AfterStepResult`] alone.
//!
//! # Runtimes
//!
//! [`ContainerRuntime`] runs one step ([`ContainerSpec`]: digest, script, env, files, mounts,
//! working directory, host working copy at [`WORKSPACE_DIR`]). [`docker::DockerRuntime`]
//! binds or copies the working copy ([`docker::WorkspaceMode`]); [`fake::FakeRuntime`] runs a
//! closure in process for tests.
//!
//! # Notes
//!
//! - Images run as their own user. The working copy is given to that user (copy mode, and
//!   the executor's Kubernetes runtime) or made writable for everyone (bind mode); see
//!   [`docker`].
//! - The host working copy and `git` on the host: the runner needs both.
//! - Steps' output is passed to the log sink unredacted; callers redact.

pub mod cache;
pub mod changeset;
pub mod docker;
mod error;
pub mod fake;
pub mod git;
mod log;
pub mod result;
mod runner;
pub mod runtime;

pub use cache::{CachedStep, MemoryStepCache, StepCache, StepCacheKey, StepInputs};
pub use changeset::{ChangesetSpecInput, SpecRepository, build_changeset_specs};
pub use error::StepsError;
pub use log::{LogSink, NoLog, StepEvent};
pub use result::{AfterStepResult, ChangedFiles};
pub use runner::{RunOutcome, StepRunner, Task, TaskRepository, parse_output, step_cache_keys};
pub use runtime::{
    ContainerFile, ContainerRuntime, ContainerSpec, HostMount, OutputStream, RuntimeError,
    WORKSPACE_DIR,
};
