//! Running the steps of one workspace.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use sourcegull_batches::spec::{BatchSpec, OutputFormat, Step};
use sourcegull_batches_template::{
    BatchChangeAttributes, Repository, StepContext, StepsContext, Value,
};
use tokio_util::sync::CancellationToken;

use crate::cache::{CachedStep, StepCache, StepCacheKey, StepInputs, fingerprint, hash_path};
use crate::error::StepsError;
use crate::git;
use crate::log::{LogSink, NoLog, StepEvent};
use crate::result::{AfterStepResult, RESULT_VERSION};
use crate::runtime::{
    ContainerFile, ContainerRuntime, ContainerSpec, HostMount, OutputStream, RuntimeError,
    WORKSPACE_DIR,
};

/// The repository a workspace is in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskRepository {
    /// `gitlab.example.com/group/project` (`repository.name`).
    pub name: String,
    /// The branch checked out, without `refs/heads/` (`repository.branch`).
    pub branch: String,
    /// The commit checked out (part of cache keys).
    pub base_rev: String,
    /// Search result paths (`repository.search_result_paths`).
    pub file_matches: Vec<String>,
}

/// One workspace's work: the spec's steps in a repository at a commit, in a directory.
#[derive(Debug, Clone)]
pub struct Task {
    /// The bulk change spec: its `steps` run; `name` and `description` are `batch_change.*`.
    pub spec: BatchSpec,
    pub repository: TaskRepository,
    /// The workspace directory relative to the repository root (`steps.path`); `""` for the
    /// root. Steps run there.
    pub path: String,
    /// `workspaces[].onlyFetchWorkspace` (part of cache keys).
    pub only_fetch_workspace: bool,
    /// Values of pass-through env variables (`env: [NAME]`): the namespace's executor secrets
    /// server-side, the local environment with `gull bulk`. Missing names are empty.
    pub global_env: HashMap<String, String>,
}

impl Task {
    fn step_context(&self) -> StepContext {
        StepContext {
            batch_change: BatchChangeAttributes {
                name: self.spec.name.clone(),
                description: self.spec.description.clone(),
            },
            repository: Repository {
                name: self.repository.name.clone(),
                branch: self.repository.branch.clone(),
                file_matches: self.repository.file_matches.clone(),
            },
            steps: StepsContext {
                path: self.path.clone(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunOutcome {
    /// The final state: the result of the last step that ran (or was restored from the cache).
    /// [`AfterStepResult::default`] (empty diff, no outputs) when no step ran.
    pub result: AfterStepResult,
    /// Results of the steps run now, in order (each was also stored in the cache).
    pub executed: Vec<AfterStepResult>,
    /// Steps skipped by their `if:`.
    pub skipped: Vec<usize>,
    /// The step the run resumed after, when the cache had a result.
    pub cache_hit: Option<usize>,
}

/// Runs bulk change spec steps in containers. Cheap to share; one runner can run many workspaces
/// concurrently.
pub struct StepRunner {
    runtime: Arc<dyn ContainerRuntime>,
    cache: Option<Arc<dyn StepCache>>,
    log: Arc<dyn LogSink>,
    mount_root: Option<PathBuf>,
    cancel: CancellationToken,
}

impl StepRunner {
    pub fn new(runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self {
            runtime,
            cache: None,
            log: Arc::new(NoLog),
            mount_root: None,
            cancel: CancellationToken::new(),
        }
    }

    /// Looks step results up before running and stores them after each step. Without a cache
    /// (the spec's `noCache`, `gull bulk --clear-cache`) every step runs.
    pub fn with_cache(mut self, cache: Arc<dyn StepCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    pub fn with_log(mut self, log: Arc<dyn LogSink>) -> Self {
        self.log = log;
        self
    }

    /// The directory `mount` paths are relative to (the bulk change spec's directory locally, the
    /// downloaded workspace files server-side). Mount paths must stay inside it. Without one,
    /// steps with `mount` fail.
    pub fn with_mount_root(mut self, dir: impl Into<PathBuf>) -> Self {
        self.mount_root = Some(dir.into());
        self
    }

    /// Cancels the run (the running container is killed).
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Runs `task`'s steps in `workspace`, a clean working copy of the repository at
    /// `task.repository.base_rev` (a git repository with that commit checked out, no other
    /// changes). Afterwards the working copy holds the final changes, staged.
    pub async fn run(&self, task: &Task, workspace: &Path) -> Result<RunOutcome, StepsError> {
        let steps = &task.spec.steps;
        let workdir = workdir(&task.path)?;

        // Images first: their digests are part of the cache keys, and a missing image fails
        // before anything runs.
        let mut digests: HashMap<&str, String> = HashMap::new();
        for step in steps {
            if digests.contains_key(step.container.as_str()) {
                continue;
            }
            let digest = self
                .runtime
                .resolve_image(&step.container, &self.cancel)
                .await
                .map_err(|e| match e {
                    RuntimeError::Canceled => StepsError::Canceled,
                    e => StepsError::Image(e),
                })?;
            self.log.event(&StepEvent::ImageResolved {
                image: &step.container,
                digest: &digest,
            });
            digests.insert(&step.container, digest);
        }
        let digest_of = |step: &Step| digests[step.container.as_str()].clone();

        let keys = match &self.cache {
            Some(_) => {
                let mut inputs = Vec::with_capacity(steps.len());
                for (i, step) in steps.iter().enumerate() {
                    inputs.push(StepInputs {
                        image_digest: digest_of(step),
                        env: step.env.resolve(&task.global_env),
                        mounts: self.hash_mounts(i, step).await?,
                    });
                }
                step_cache_keys(task, &inputs)
            }
            None => Vec::new(),
        };

        let mut outcome = RunOutcome::default();
        let mut outputs: BTreeMap<String, Value> = BTreeMap::new();
        let mut previous = AfterStepResult::default();
        let mut start = 0;
        if let Some(hit) = self.find_cached(&keys).await {
            let k = hit.step_index;
            self.log.event(&StepEvent::CacheHit { step: k });
            outcome.cache_hit = Some(k);
            outputs = hit.outputs.clone();
            start = k + 1;
            if start < steps.len() {
                git::apply(workspace, &hit.diff).await?;
            }
            previous = hit.clone();
            outcome.result = hit;
        }

        for (i, step) in steps.iter().enumerate().skip(start) {
            if self.cancel.is_cancelled() {
                return Err(StepsError::Canceled);
            }
            let mut ctx = task.step_context();
            ctx.outputs = outputs.clone();
            ctx.steps.changes = previous.changed_files.to_template();
            ctx.previous_step = previous.to_template();
            let template_err = |field: String| {
                move |error| StepsError::Template {
                    step: i,
                    field,
                    error: Box::new(error),
                }
            };

            if let Some(cond) = step.if_condition() {
                let run = ctx
                    .eval_condition(&cond)
                    .map_err(template_err("if".into()))?;
                if !run {
                    self.log.event(&StepEvent::Skipped { step: i });
                    outcome.skipped.push(i);
                    continue;
                }
            }

            let script = ctx.render(&step.run).map_err(template_err("run".into()))?;
            let mut env = BTreeMap::new();
            for (name, value) in step.env.resolve(&task.global_env) {
                let rendered = ctx
                    .render(&value)
                    .map_err(template_err(format!("env.{name}")))?;
                env.insert(name, rendered);
            }
            let mut files = Vec::with_capacity(step.files.len());
            for (target, content) in &step.files {
                let rendered = ctx
                    .render(content)
                    .map_err(template_err(format!("files.{target}")))?;
                files.push(ContainerFile {
                    target: container_path(i, target)?,
                    content: rendered.into_bytes(),
                });
            }
            let mut mounts = Vec::with_capacity(step.mount.len());
            for m in &step.mount {
                mounts.push(HostMount {
                    source: self.mount_source(i, &m.path)?,
                    target: container_path(i, &m.mountpoint)?,
                });
            }
            let digest = digest_of(step);
            let spec = ContainerSpec {
                step: i,
                image: step.container.clone(),
                digest: digest.clone(),
                workspace: workspace.to_owned(),
                workdir: workdir.clone(),
                script,
                env,
                files,
                mounts,
            };
            self.log.event(&StepEvent::Started {
                step: i,
                image: &step.container,
                digest: &digest,
                script: &spec.script,
            });

            let buffers = Mutex::new((Vec::new(), Vec::new()));
            let on_output = |stream: OutputStream, data: &[u8]| {
                {
                    let mut b = buffers.lock().unwrap_or_else(|e| e.into_inner());
                    match stream {
                        OutputStream::Stdout => b.0.extend_from_slice(data),
                        OutputStream::Stderr => b.1.extend_from_slice(data),
                    }
                }
                self.log.event(&StepEvent::Output {
                    step: i,
                    stream,
                    data,
                });
            };
            let exit_code = self
                .runtime
                .run(&spec, &on_output, &self.cancel)
                .await
                .map_err(|e| match e {
                    RuntimeError::Canceled => StepsError::Canceled,
                    error => StepsError::Runtime { step: i, error },
                })?;
            let (stdout, stderr) = buffers.into_inner().unwrap_or_else(|e| e.into_inner());
            let stdout = String::from_utf8_lossy(&stdout).into_owned();
            let stderr = String::from_utf8_lossy(&stderr).into_owned();
            self.log.event(&StepEvent::Finished { step: i, exit_code });
            if exit_code != 0 {
                return Err(StepsError::StepFailed {
                    step: i,
                    exit_code,
                    stdout,
                    stderr,
                });
            }

            let changed_files = git::changes(workspace).await?;
            let mut result = AfterStepResult {
                version: RESULT_VERSION,
                step_index: i,
                diff: Vec::new(),
                changed_files,
                stdout,
                stderr,
                outputs: BTreeMap::new(),
            };
            // Outputs see the step's own result (`step.*`) and the outputs set before them,
            // including earlier ones of this step (in name order).
            ctx.step = result.to_template();
            for (name, output) in &step.outputs {
                let rendered = ctx
                    .render(&output.value)
                    .map_err(template_err(format!("outputs.{name}")))?;
                let value = parse_output(&rendered, output.format()).map_err(|message| {
                    StepsError::Output {
                        step: i,
                        name: name.clone(),
                        message,
                    }
                })?;
                ctx.outputs.insert(name.clone(), value.clone());
                outputs.insert(name.clone(), value);
            }
            result.outputs = outputs.clone();
            result.diff = git::diff(workspace).await?;

            if let (Some(cache), Some(key)) = (&self.cache, keys.get(i)) {
                let entry = CachedStep {
                    fingerprint: key.fingerprint.clone(),
                    result: result.clone(),
                };
                if let Err(e) = cache.set(&key.key, &entry).await {
                    self.log.event(&StepEvent::Warning {
                        message: &format!("storing the result of step {}: {e}", i + 1),
                    });
                }
            }
            outcome.executed.push(result.clone());
            previous = result.clone();
            outcome.result = result;
        }
        Ok(outcome)
    }

    /// The last step with a usable cache entry, searching from the last step backwards.
    async fn find_cached(&self, keys: &[StepCacheKey]) -> Option<AfterStepResult> {
        let cache = self.cache.as_ref()?;
        for (i, key) in keys.iter().enumerate().rev() {
            match cache.get(&key.key).await {
                Ok(Some(entry))
                    if entry.fingerprint == key.fingerprint && entry.result.step_index == i =>
                {
                    return Some(entry.result);
                }
                Ok(_) => {}
                Err(e) => self.log.event(&StepEvent::Warning {
                    message: &format!("reading the cached result of step {}: {e}", i + 1),
                }),
            }
        }
        None
    }

    async fn hash_mounts(
        &self,
        step: usize,
        s: &Step,
    ) -> Result<Vec<(String, String)>, StepsError> {
        let mut out = Vec::with_capacity(s.mount.len());
        for m in &s.mount {
            let source = self.mount_source(step, &m.path)?;
            let hash = tokio::task::spawn_blocking(move || hash_path(&source))
                .await
                .map_err(|e| StepsError::Io(e.to_string()))?
                .map_err(|e| StepsError::InvalidPath {
                    step,
                    message: format!("reading mount {}: {e}", m.path),
                })?;
            out.push((m.path.clone(), hash));
        }
        Ok(out)
    }

    /// The host path of a `mount` entry: inside the mount root.
    fn mount_source(&self, step: usize, path: &str) -> Result<PathBuf, StepsError> {
        let invalid = |message: String| StepsError::InvalidPath { step, message };
        let Some(root) = &self.mount_root else {
            return Err(invalid(format!(
                "cannot mount {path}: mounts are not available here"
            )));
        };
        let p = Path::new(path);
        let rel = if p.is_absolute() {
            p.strip_prefix(root)
                .map_err(|_| invalid(format!("mount path {path} is not in {}", root.display())))?
        } else {
            p
        };
        if rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(invalid(format!(
                "mount path {path} is not in the bulk change spec's directory"
            )));
        }
        let source = root.join(rel);
        if !source.exists() {
            return Err(invalid(format!("mount path {path} does not exist")));
        }
        Ok(source)
    }
}

/// The cache keys of every step of `task` (see [`crate::cache`]); `inputs` has one entry per
/// step.
pub fn step_cache_keys(task: &Task, inputs: &[StepInputs]) -> Vec<StepCacheKey> {
    (0..task.spec.steps.len().min(inputs.len()))
        .map(|i| StepCacheKey {
            key: sourcegull_batches::cache_key::step_cache_key(
                &task.spec,
                &task.repository.name,
                &task.repository.base_rev,
                &task.repository.file_matches,
                &task.path,
                task.only_fetch_workspace,
                i,
            ),
            fingerprint: fingerprint(inputs, i),
        })
        .collect()
}

/// The working directory in containers for a workspace path.
pub fn workdir(path: &str) -> Result<String, StepsError> {
    let mut dir = WORKSPACE_DIR.to_owned();
    for seg in path.split('/').filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            return Err(StepsError::InvalidPath {
                step: 0,
                message: format!("workspace path {path:?} is outside the repository"),
            });
        }
        dir.push('/');
        dir.push_str(seg);
    }
    Ok(dir)
}

/// A `files` or `mount` target: absolute, without `..`.
fn container_path(step: usize, path: &str) -> Result<String, StepsError> {
    if !path.starts_with('/') || path.split('/').any(|s| s == "..") {
        return Err(StepsError::InvalidPath {
            step,
            message: format!("container path {path:?} must be absolute"),
        });
    }
    Ok(path.to_owned())
}

/// Parses a rendered output: text as is; JSON and YAML decode their first value (empty text
/// fails with `EOF`). JSON numbers are floats; YAML integers are integers.
pub fn parse_output(rendered: &str, format: OutputFormat) -> Result<Value, String> {
    match format {
        OutputFormat::Text => Ok(Value::String(rendered.to_owned())),
        OutputFormat::Json => {
            let mut stream =
                serde_json::Deserializer::from_str(rendered).into_iter::<serde_json::Value>();
            match stream.next() {
                None => Err("EOF".into()),
                Some(Err(e)) => Err(e.to_string()),
                Some(Ok(v)) => Ok(Value::from_json(&v)),
            }
        }
        OutputFormat::Yaml => {
            if rendered.trim().is_empty() {
                return Err("EOF".into());
            }
            let (v, _) =
                sourcegull_batches::yaml::load(rendered).map_err(|e| match e.location {
                    Some(l) => format!("{} at {l}", e.message),
                    None => e.message,
                })?;
            Ok(Value::from_json_yaml_numbers(&v))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs() {
        assert_eq!(
            parse_output(" x \n", OutputFormat::Text).unwrap(),
            Value::from(" x \n")
        );
        assert_eq!(
            parse_output("{\"a\": [1, \"b\"]} trailing", OutputFormat::Json).unwrap(),
            Value::map([("a", Value::List(vec![Value::Float(1.0), Value::from("b")]))])
        );
        assert_eq!(
            parse_output("a: 1\nb: [x, 2.5]\n", OutputFormat::Yaml).unwrap(),
            Value::map([
                ("a", Value::Int(1)),
                ("b", Value::List(vec![Value::from("x"), Value::Float(2.5)])),
            ])
        );
        assert!(parse_output("", OutputFormat::Json).is_err());
        assert!(parse_output("  \n", OutputFormat::Yaml).is_err());
        assert!(parse_output("{", OutputFormat::Json).is_err());
    }

    #[test]
    fn workdirs() {
        assert_eq!(workdir("").unwrap(), "/work");
        assert_eq!(workdir("a/./b/").unwrap(), "/work/a/b");
        assert!(workdir("../x").is_err());
        assert!(container_path(0, "tmp/x").is_err());
        assert!(container_path(0, "/tmp/../x").is_err());
    }
}
