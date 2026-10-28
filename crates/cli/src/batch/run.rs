//! `gull bulk preview|apply`: the local run (see the module docs of [`super`]).

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use sourcegull_batches::spec::BatchSpec;
use sourcegull_batches::{ChangesetSpec, ExistingChangesetSpec};
use sourcegull_batches_steps::{
    ChangesetSpecInput, ContainerRuntime, SpecRepository, StepEvent, StepRunner, StepsError, Task,
    TaskRepository, build_changeset_specs,
};
use tokio_util::sync::CancellationToken;

use super::cache::{DiskStepCache, default_dir};
use super::client::{self, Client};
use super::workspace::{self, Checkout};
use super::{ExecuteArgs, read_spec, validate_spec};
use crate::env::{Env, Instance};

/// What a run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The bulk change spec's GraphQL ID.
    pub batch_spec_id: String,
    /// Absolute URL of the preview (`applyURL`).
    pub preview_url: String,
    /// Absolute URL of the bulk change, after `apply`.
    pub batch_change_url: Option<String>,
    /// GraphQL IDs of the uploaded changeset specs.
    pub changeset_specs: Vec<String>,
    /// Workspaces that ran (or were restored from the cache).
    pub workspaces: usize,
    /// Errors of workspaces left out with `-skip-errors`.
    pub errors: Vec<String>,
}

/// One workspace as `resolveWorkspacesForBatchSpec` returns it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedWorkspace {
    pub repository: ResolvedRepository,
    pub branch: ResolvedBranch,
    pub path: String,
    pub only_fetch_workspace: bool,
    pub ignored: bool,
    pub unsupported: bool,
    #[serde(default)]
    pub search_result_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolvedRepository {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedBranch {
    /// `refs/heads/main`.
    pub name: String,
    /// `main`.
    pub abbrev_name: String,
    pub target: Target,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub oid: String,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

/// Prints what the run does (standard error; standard output gets the final URLs).
fn say(line: impl AsRef<str>) {
    eprintln!("{}", line.as_ref());
}

/// The user or organization namespace named `name`, else the current user.
pub async fn resolve_namespace(client: &Client, name: Option<&str>) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Current {
        current_user: Option<Id>,
    }
    #[derive(Deserialize)]
    struct Named {
        user: Option<Id>,
        organization: Option<Id>,
    }
    match name.filter(|n| !n.is_empty()) {
        None => {
            let c: Current = client
                .graphql(client::CURRENT_USER_QUERY, json!({}))
                .await?;
            c.current_user.map(|u| u.id).ok_or_else(|| {
                anyhow::anyhow!("not signed in: set SOURCEGULL_ACCESS_TOKEN (or SRC_ACCESS_TOKEN)")
            })
        }
        Some(n) => {
            let r: Named = client
                .graphql(client::NAMESPACE_QUERY, json!({ "name": n }))
                .await?;
            r.user.or(r.organization).map(|u| u.id).ok_or_else(|| {
                anyhow::anyhow!("namespace {n:?} not found: no user or organization has that name")
            })
        }
    }
}

/// The workspaces to run in, leaving out unsupported and ignored repositories unless
/// allowed (with a message).
pub async fn resolve_workspaces(
    client: &Client,
    raw: &str,
    allow_unsupported: bool,
    allow_ignored: bool,
) -> anyhow::Result<Vec<ResolvedWorkspace>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        resolve_workspaces_for_batch_spec: Vec<ResolvedWorkspace>,
    }
    let r: Resp = client
        .graphql(client::RESOLVE_WORKSPACES_QUERY, json!({ "spec": raw }))
        .await
        .map_err(|e| anyhow::anyhow!("resolving workspaces: {e}"))?;
    let mut unsupported = Vec::new();
    let mut ignored = Vec::new();
    let mut out = Vec::new();
    for w in r.resolve_workspaces_for_batch_spec {
        if w.unsupported && !allow_unsupported {
            unsupported.push(w.repository.name);
        } else if w.ignored && !allow_ignored {
            ignored.push(w.repository.name);
        } else {
            out.push(w);
        }
    }
    for (names, what, flag) in [
        (
            unsupported,
            "on unsupported code hosts",
            "-allow-unsupported",
        ),
        (ignored, "with a .batchignore file", "-allow-ignored"),
    ] {
        let mut names = names;
        names.sort();
        names.dedup();
        if !names.is_empty() {
            say(format!(
                "Skipping {} {} {what} (use {flag} to include them):",
                names.len(),
                if names.len() == 1 {
                    "repository"
                } else {
                    "repositories"
                },
            ));
            for n in names {
                say(format!("  {n}"));
            }
        }
    }
    Ok(out)
}

/// The `existing` changeset specs of `importChangesets`.
pub async fn import_specs(client: &Client, spec: &BatchSpec) -> anyhow::Result<Vec<ChangesetSpec>> {
    #[derive(Deserialize)]
    struct Resp {
        repository: Option<Id>,
    }
    let mut out = Vec::new();
    for import in &spec.import_changesets {
        let r: Resp = client
            .graphql(
                client::REPOSITORY_QUERY,
                json!({ "name": import.repository }),
            )
            .await?;
        let repo = r
            .repository
            .ok_or_else(|| anyhow::anyhow!("repository {:?} not found", import.repository))?;
        for id in &import.external_ids {
            out.push(ChangesetSpec::Existing(ExistingChangesetSpec {
                version: None,
                base_repository: repo.id.clone(),
                external_id: id.to_string(),
            }));
        }
    }
    Ok(out)
}

/// Fails when two branch specs target the same branch of a repository.
pub fn check_duplicate_branches(
    specs: &[ChangesetSpec],
    names: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let mut seen: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for s in specs {
        if let ChangesetSpec::Branch(b) = s {
            *seen
                .entry((b.base_repository.as_str(), b.head_ref.as_str()))
                .or_default() += 1;
        }
    }
    let dups: Vec<String> = seen
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|((repo, branch), n)| {
            let name = names.get(repo).map(String::as_str).unwrap_or(repo);
            format!("  - {name}: {n} specs push to branch {branch:?}")
        })
        .collect();
    if dups.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "Some changeset specs would push to the same branch of a repository:\n{}\nGive each \
             changeset of a repository its own branch (changesetTemplate.branch or \
             transformChanges.group).",
            dups.join("\n")
        )
    }
}

/// A workspace's log file (written as the steps run).
struct LogFile {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl LogFile {
    fn create(dir: &Path, name: &str) -> Self {
        let path = dir.join(format!("gull-batch-{name}.log"));
        let file = std::fs::File::create(&path).ok();
        Self {
            path,
            file: Mutex::new(file),
        }
    }

    fn write(&self, data: &[u8]) {
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = f.write_all(data);
        }
    }

    fn event(&self, e: &StepEvent<'_>) {
        match e {
            StepEvent::ImageResolved { image, digest } => {
                self.write(format!("image {image}: {digest}\n").as_bytes());
            }
            StepEvent::CacheHit { step } => {
                self.write(format!("steps 1-{} restored from the cache\n", step + 1).as_bytes());
            }
            StepEvent::Skipped { step } => {
                self.write(format!("step {}: skipped (if: false)\n", step + 1).as_bytes());
            }
            StepEvent::Started {
                step,
                image,
                script,
                ..
            } => self.write(format!("step {} in {image}:\n{script}\n", step + 1).as_bytes()),
            StepEvent::Output { data, .. } => self.write(data),
            StepEvent::Finished { step, exit_code } => {
                self.write(format!("step {}: exit code {exit_code}\n", step + 1).as_bytes());
            }
            StepEvent::Warning { message } => {
                self.write(format!("warning: {message}\n").as_bytes());
            }
        }
    }
}

/// Everything a workspace run needs.
struct Shared<'a> {
    client: &'a Client,
    runtime: Arc<dyn ContainerRuntime>,
    cache: Arc<DiskStepCache>,
    spec: &'a BatchSpec,
    mount_root: &'a Path,
    tmp: &'a Path,
    global_env: HashMap<String, String>,
    timeout: Duration,
    stop: CancellationToken,
}

/// The result of one workspace.
struct WorkspaceResult {
    label: String,
    specs: Result<Vec<ChangesetSpec>, String>,
    log: PathBuf,
}

fn label(w: &ResolvedWorkspace) -> String {
    let mut s = w.repository.name.clone();
    if !w.path.is_empty() {
        s.push_str(&format!(" ({})", w.path));
    }
    if !w.branch.abbrev_name.is_empty() {
        s.push_str(&format!(" @ {}", w.branch.abbrev_name));
    }
    s
}

/// A file name for a workspace's log and working copy.
fn slug(w: &ResolvedWorkspace, n: usize) -> String {
    let raw = format!("{}-{}-{n}", w.repository.name, w.path);
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

async fn run_workspace(sh: &Shared<'_>, w: &ResolvedWorkspace, n: usize) -> WorkspaceResult {
    let label = label(w);
    let log = Arc::new(LogFile::create(sh.tmp, &slug(w, n)));
    let result = run_workspace_inner(sh, w, log.clone()).await;
    WorkspaceResult {
        label,
        specs: result.map_err(|e| format!("{e:#}")),
        log: log.path.clone(),
    }
}

async fn run_workspace_inner(
    sh: &Shared<'_>,
    w: &ResolvedWorkspace,
    log: Arc<LogFile>,
) -> anyhow::Result<Vec<ChangesetSpec>> {
    if sh.stop.is_cancelled() {
        anyhow::bail!("canceled");
    }
    let cancel = sh.stop.child_token();
    let timer = {
        let cancel = cancel.clone();
        let timeout = sh.timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            cancel.cancel();
        })
    };
    let outcome = async {
        let dir = tempfile::Builder::new()
            .prefix("gull-batch-")
            .tempdir_in(sh.tmp)?;
        let checkout = Checkout {
            repo: &w.repository.name,
            commit: &w.branch.target.oid,
            path: &w.path,
            only_fetch_workspace: w.only_fetch_workspace,
        };
        tokio::select! {
            r = workspace::prepare(sh.client, &checkout, dir.path()) => r?,
            () = cancel.cancelled() => anyhow::bail!("canceled"),
        }
        let task = Task {
            spec: sh.spec.clone(),
            repository: TaskRepository {
                name: w.repository.name.clone(),
                branch: w.branch.abbrev_name.clone(),
                base_rev: w.branch.target.oid.clone(),
                file_matches: w.search_result_paths.clone(),
            },
            path: w.path.clone(),
            only_fetch_workspace: w.only_fetch_workspace,
            global_env: sh.global_env.clone(),
        };
        let sink = log.clone();
        let runner = StepRunner::new(sh.runtime.clone())
            .with_cache(sh.cache.clone())
            .with_mount_root(sh.mount_root)
            .with_cancel(cancel.clone())
            .with_log(Arc::new(move |e: &StepEvent<'_>| sink.event(e)));
        let outcome = runner.run(&task, dir.path()).await?;
        let repository = SpecRepository {
            id: w.repository.id.clone(),
            name: w.repository.name.clone(),
            base_ref: w.branch.name.clone(),
            base_rev: w.branch.target.oid.clone(),
            file_matches: w.search_result_paths.clone(),
        };
        let specs = build_changeset_specs(&ChangesetSpecInput {
            spec: sh.spec,
            repository: &repository,
            path: &w.path,
            result: &outcome.result,
            fallback_author: None,
        })?;
        anyhow::Ok(specs)
    }
    .await;
    timer.abort();
    match outcome {
        Err(e)
            if cancel.is_cancelled()
                && !sh.stop.is_cancelled()
                && (matches!(e.downcast_ref::<StepsError>(), Some(StepsError::Canceled))
                    || e.to_string() == "canceled") =>
        {
            anyhow::bail!(
                "the workspace ran longer than the -timeout of {:?} and was stopped",
                sh.timeout
            )
        }
        other => other,
    }
}

/// Uploads changeset specs in batches of at most [`client::UPLOAD_BATCH_BYTES`].
pub async fn upload_specs(client: &Client, specs: &[ChangesetSpec]) -> anyhow::Result<Vec<String>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        create_changeset_specs: Vec<Id>,
    }
    let mut ids = Vec::with_capacity(specs.len());
    let mut batch: Vec<String> = Vec::new();
    let mut size = 0;
    let mut pending = specs.iter().map(ChangesetSpec::to_json).peekable();
    while let Some(json) = pending.next() {
        size += json.len();
        batch.push(json);
        let full = pending
            .peek()
            .is_none_or(|next| size + next.len() > client::UPLOAD_BATCH_BYTES);
        if full {
            let r: Resp = client
                .graphql(
                    client::CREATE_CHANGESET_SPECS_MUTATION,
                    json!({ "specs": std::mem::take(&mut batch) }),
                )
                .await
                .map_err(|e| anyhow::anyhow!("uploading changeset specs: {e}"))?;
            ids.extend(r.create_changeset_specs.into_iter().map(|i| i.id));
            size = 0;
        }
    }
    Ok(ids)
}

/// Runs `preview` (`apply` false) or `apply` with containers on `runtime`.
pub async fn execute(
    args: &ExecuteArgs,
    apply: bool,
    env: &dyn Env,
    runtime: Arc<dyn ContainerRuntime>,
) -> anyhow::Result<Outcome> {
    let source = read_spec(args.spec_file()?)?;
    let parsed = validate_spec(&source)?;
    for w in &parsed.warnings {
        say(format!("warning: {w}"));
    }
    let spec = parsed.spec;
    let instance = Instance::from_env(env)?;
    let client = Client::new(instance.clone(), args.insecure_skip_verify)?;

    say("Looking up the namespace");
    let namespace = resolve_namespace(&client, args.namespace.as_deref()).await?;
    say("Finding workspaces");
    let workspaces = resolve_workspaces(
        &client,
        &source.raw,
        args.allow_unsupported,
        args.allow_ignored,
    )
    .await?;
    let imports = import_specs(&client, &spec).await?;

    let tmp = match &args.tmp {
        Some(t) => t.clone(),
        None => std::env::temp_dir(),
    };
    std::fs::create_dir_all(&tmp)?;
    let parallelism = args
        .parallelism
        .filter(|j| *j > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let cache_dir = args.cache.clone().unwrap_or_else(|| default_dir(env));
    let global_env: HashMap<String, String> = spec
        .steps
        .iter()
        .flat_map(|s| s.env.outer_vars())
        .filter_map(|name| env.var(name).map(|v| (name.to_owned(), v)))
        .collect();
    let shared = Shared {
        client: &client,
        runtime,
        cache: Arc::new(DiskStepCache::new(cache_dir, !args.clear_cache)),
        spec: &spec,
        mount_root: &source.dir,
        tmp: &tmp,
        global_env,
        timeout: args.timeout,
        stop: CancellationToken::new(),
    };
    if !workspaces.is_empty() {
        say(format!(
            "Running steps in {} {} ({} at a time)",
            workspaces.len(),
            if workspaces.len() == 1 {
                "workspace"
            } else {
                "workspaces"
            },
            parallelism.min(workspaces.len())
        ));
    }
    let mut results: Vec<(usize, WorkspaceResult)> =
        futures::stream::iter(workspaces.iter().enumerate())
            .map(|(n, w)| {
                let shared = &shared;
                async move {
                    let r = run_workspace(shared, w, n).await;
                    match &r.specs {
                        Ok(specs) => say(format!(
                            "  ✓ {}: {} changeset {}",
                            r.label,
                            specs.len(),
                            if specs.len() == 1 { "spec" } else { "specs" }
                        )),
                        Err(e) => {
                            say(format!("  ✗ {}: {e}", r.label));
                            if args.fail_fast {
                                shared.stop.cancel();
                            }
                        }
                    }
                    (n, r)
                }
            })
            .buffer_unordered(parallelism)
            .collect()
            .await;
    results.sort_by_key(|(n, _)| *n);

    let mut specs: Vec<ChangesetSpec> = Vec::new();
    let mut errors = Vec::new();
    for (_, r) in results {
        match r.specs {
            Ok(s) => {
                specs.extend(s);
                if !args.keep_logs {
                    let _ = std::fs::remove_file(&r.log);
                }
            }
            Err(e) => errors.push(format!("{}: {e} (log: {})", r.label, r.log.display())),
        }
    }
    if !errors.is_empty() {
        let message = format!(
            "{} {} failed:\n\t* {}",
            errors.len(),
            if errors.len() == 1 {
                "workspace"
            } else {
                "workspaces"
            },
            errors.join("\n\t* ")
        );
        if !args.skip_errors {
            anyhow::bail!("{message}\n\nUse -skip-errors to upload the changes of the others.");
        }
        say(format!("{message}\n\nSkipping them (-skip-errors)."));
    }
    let names: HashMap<String, String> = workspaces
        .iter()
        .map(|w| (w.repository.id.clone(), w.repository.name.clone()))
        .collect();
    check_duplicate_branches(&specs, &names)?;
    specs.extend(imports);

    if !specs.is_empty() {
        say(format!(
            "Uploading {} changeset {}",
            specs.len(),
            if specs.len() == 1 { "spec" } else { "specs" }
        ));
    }
    let ids = upload_specs(&client, &specs).await?;
    say("Creating bulk change spec on the instance");
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Created {
        create_batch_spec: CreatedSpec,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CreatedSpec {
        id: String,
        #[serde(rename = "applyURL")]
        apply_url: String,
    }
    let created: Created = client
        .graphql(
            client::CREATE_BATCH_SPEC_MUTATION,
            json!({ "namespace": namespace, "spec": source.raw, "changesetSpecs": ids }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("creating the bulk change spec: {e}"))?;
    let preview_url = absolute(&instance, &created.create_batch_spec.apply_url);
    let mut outcome = Outcome {
        batch_spec_id: created.create_batch_spec.id,
        preview_url,
        batch_change_url: None,
        changeset_specs: ids,
        workspaces: workspaces.len(),
        errors,
    };
    if apply {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Applied {
            apply_batch_change: AppliedChange,
        }
        #[derive(Deserialize)]
        struct AppliedChange {
            url: String,
        }
        say("Applying the bulk change spec");
        let applied: Applied = client
            .graphql(
                client::APPLY_BATCH_CHANGE_MUTATION,
                json!({ "batchSpec": outcome.batch_spec_id }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("applying the bulk change spec: {e}"))?;
        let url = absolute(&instance, &applied.apply_batch_change.url);
        println!("Applied. The bulk change:\n{url}");
        outcome.batch_change_url = Some(url);
    } else {
        println!(
            "Uploaded. Preview and apply it at:\n{}",
            outcome.preview_url
        );
    }
    Ok(outcome)
}

fn absolute(instance: &Instance, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_owned()
    } else {
        instance.url(url)
    }
}
