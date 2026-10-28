//! Server-side execution of bulk change specs over GraphQL: the mutations
//! `executeBatchSpec`, `cancelBatchSpecExecution`, `retryBatchSpecExecution`,
//! `retryBatchSpecWorkspaceExecution` and `enqueueBatchSpecWorkspaceExecution`, and what a
//! `VisibleBatchSpecWorkspace` shows of its execution: `stages` (the executor's log entries)
//! and `steps` (`BatchSpecWorkspaceStep`, from the log, the execution cache and the spec).
//!
//! Only the spec's creator and site admins execute, cancel and retry. The
//! work itself is in [`sourcegull_services::batches`], shared with the executor queue.
//!
//! The executor's log keys (see `sourcegull_executor_api`, "The `batches` queue"): `setup.*`,
//! `step.<i>` (step `i` from 0 ran), `step.<i>.skipped`, `step.<i>.cached` (steps `0..=i`
//! came from the cache) and `teardown.*`. Logs are shown to those who may administer the
//! spec's namespace and to site admins; others see the entries without their output.

use std::collections::{BTreeMap, HashMap};

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Duration, Utc};
use sourcegull_authn::Actor;
use sourcegull_batches::BatchSpec as ParsedSpec;
use sourcegull_batches::store::{
    BatchSpec, BatchSpecStore, BatchSpecWorkspaceStore, ExecutionCacheStore, ExecutionJobRow,
};
use sourcegull_batches_steps::{AfterStepResult, CachedStep};
use sourcegull_pgqueue::{ExecutionLogEntry, JobState};
use sourcegull_services::batches::{ExecuteError, ExecuteOptions, access, step_cache_keys};
use sourcegull_services::relay::unmarshal_int_id;
use sourcegull_types::Repo;

use super::batch_spec::{BatchSpecNode, BatchSpecStateGql, WorkspaceData, spec_by_id};
use super::{JsonValue, WORKSPACE_KIND, invalid, invalid_id, require_write};
use crate::context::{access_error, codes, db_error, error, services, to_int, viewer};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::diff::DiffStat;
use sourcegull_graphql_core::execution::ExecutionLogEntryNode;
use sourcegull_graphql_core::shared::ExecutorRef;

/// Most failure messages listed in a spec's `failureMessage`.
const MAX_FAILURES: i64 = 10;
/// Default and largest page of `outputLines`.
const MAX_OUTPUT_LINES: i64 = 10_000;

/// Fails unless the viewer created the spec or is a site admin.
fn require_can_modify_spec(actor: &Actor, spec: &BatchSpec) -> Result<()> {
    access::require_can_modify_spec(actor, spec).map_err(access_error)
}

/// Whether the viewer may retry or cancel `spec`'s execution.
pub(super) fn viewer_can_modify(ctx: &Context<'_>, spec: &BatchSpec) -> bool {
    viewer(ctx).is_some_and(|a| require_can_modify_spec(a, spec).is_ok())
}

/// Whether the viewer may read `spec`'s execution logs: its creator, those who may administer
/// its namespace, and site admins.
pub(super) async fn viewer_can_see_logs(ctx: &Context<'_>, spec: &BatchSpec) -> Result<bool> {
    access::can_see_spec_details(&services(ctx)?.db, viewer(ctx), spec)
        .await
        .map_err(db_error)
}

fn execute_error(e: ExecuteError) -> async_graphql::Error {
    match e {
        ExecuteError::Db(e) => db_error(e),
        e => invalid(e.to_string()),
    }
}

/// What applies specs executed with `autoApplyEnabled` (`sourcegull_graphql::batches::auto_apply`).
fn auto_applier(
    services: &sourcegull_graphql_core::Services,
) -> Option<&std::sync::Arc<dyn sourcegull_services::batches::AutoApplier>> {
    services.app.as_ref()?.auto_applier.as_ref()
}

async fn reload(ctx: &Context<'_>, id: i64) -> Result<BatchSpecNode> {
    BatchSpecStore::new(&services(ctx)?.db)
        .get(id)
        .await
        .map_err(db_error)?
        .map(BatchSpecNode::new)
        .ok_or_else(|| error(codes::NOT_FOUND, "Bulk change spec not found."))
}

/// `executeBatchSpec`.
pub(super) async fn execute(
    ctx: &Context<'_>,
    id: &str,
    no_cache: Option<bool>,
    auto_apply: Option<bool>,
) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = spec_by_id(ctx, id).await?;
    require_can_modify_spec(actor, &spec)?;
    let services = services(ctx)?;
    sourcegull_services::batches::execute_batch_spec(
        &services.db,
        auto_applier(services),
        &spec,
        ExecuteOptions {
            no_cache,
            auto_apply: auto_apply.unwrap_or(false),
        },
    )
    .await
    .map_err(execute_error)?;
    reload(ctx, spec.id).await
}

/// `cancelBatchSpecExecution`: queued executions are canceled at once, running ones when
/// their executor sees the request at its next heartbeat.
pub(super) async fn cancel(ctx: &Context<'_>, id: &str) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = spec_by_id(ctx, id).await?;
    require_can_modify_spec(actor, &spec)?;
    let node = BatchSpecNode::new(spec.clone());
    if !matches!(
        node.compute_state(ctx).await?,
        BatchSpecStateGql::Queued | BatchSpecStateGql::Processing
    ) {
        return Err(invalid(
            "The bulk change spec's execution is not queued or processing, so it cannot be canceled.",
        ));
    }
    let db = &services(ctx)?.db;
    BatchSpecWorkspaceStore::new(db)
        .cancel_executions(spec.id)
        .await
        .map_err(db_error)?;
    sourcegull_services::batches::execution_finished(db, auto_applier(services(ctx)?), spec.id)
        .await
        .map_err(db_error)?;
    reload(ctx, spec.id).await
}

/// `retryBatchSpecExecution`: runs the failed and canceled executions again (and the
/// completed ones with `includeCompleted`) once the execution is over.
pub(super) async fn retry(
    ctx: &Context<'_>,
    id: &str,
    include_completed: bool,
) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = spec_by_id(ctx, id).await?;
    require_can_modify_spec(actor, &spec)?;
    if spec.execution_requested_at.is_none() {
        return Err(invalid("The bulk change spec has not been executed."));
    }
    let node = BatchSpecNode::new(spec.clone());
    if !matches!(
        node.compute_state(ctx).await?,
        BatchSpecStateGql::Completed | BatchSpecStateGql::Failed | BatchSpecStateGql::Canceled
    ) {
        return Err(invalid(
            "The bulk change spec's execution has not finished, so it cannot be retried.",
        ));
    }
    let db = &services(ctx)?.db;
    BatchSpecWorkspaceStore::new(db)
        .retry_executions(spec.id, None, include_completed)
        .await
        .map_err(db_error)?;
    BatchSpecStore::new(db)
        .reset_auto_apply(spec.id)
        .await
        .map_err(db_error)?;
    reload(ctx, spec.id).await
}

/// The workspaces behind IDs with their spec (all must belong to one spec, which the viewer
/// may modify) and latest executions by workspace id.
async fn workspaces_of(
    ctx: &Context<'_>,
    actor: &Actor,
    ids: &[ID],
) -> Result<(BatchSpec, Vec<i64>, HashMap<i64, ExecutionJobRow>)> {
    if ids.is_empty() {
        return Err(invalid("No workspaces given."));
    }
    let db = &services(ctx)?.db;
    let mut keys = Vec::with_capacity(ids.len());
    let mut spec_id = None;
    for id in ids {
        let key = unmarshal_int_id(WORKSPACE_KIND, id).ok_or_else(|| invalid_id(id))?;
        let ws = BatchSpecWorkspaceStore::new(db)
            .get(key)
            .await
            .map_err(db_error)?
            .ok_or_else(|| error(codes::NOT_FOUND, "Workspace not found."))?;
        if spec_id.is_some_and(|s| s != ws.batch_spec_id) {
            return Err(invalid(
                "The workspaces must belong to one bulk change spec.",
            ));
        }
        spec_id = Some(ws.batch_spec_id);
        keys.push(ws.id);
    }
    let spec = BatchSpecStore::new(db)
        .get(spec_id.unwrap_or_default())
        .await
        .map_err(db_error)?
        .ok_or_else(|| error(codes::NOT_FOUND, "Bulk change spec not found."))?;
    require_can_modify_spec(actor, &spec)?;
    let jobs = BatchSpecWorkspaceStore::new(db)
        .execution_jobs_for_workspaces(&keys)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|j| (j.payload.batch_spec_workspace_id, j))
        .collect();
    Ok((spec, keys, jobs))
}

fn finished(state: JobState) -> bool {
    matches!(
        state,
        JobState::Completed | JobState::Failed | JobState::Canceled
    )
}

/// `retryBatchSpecWorkspaceExecution`: runs the executions of the given workspaces again
/// (each must have finished).
pub(super) async fn retry_workspaces(ctx: &Context<'_>, ids: &[ID]) -> Result<EmptyResponse> {
    let actor = require_write(ctx)?;
    let (spec, keys, jobs) = workspaces_of(ctx, actor, ids).await?;
    let mut job_ids = Vec::with_capacity(keys.len());
    for key in &keys {
        match jobs.get(key) {
            Some(j) if finished(j.state) => job_ids.push(j.id),
            Some(_) => return Err(invalid("A workspace's execution has not finished.")),
            None => return Err(invalid("A workspace has not been executed.")),
        }
    }
    let db = &services(ctx)?.db;
    BatchSpecWorkspaceStore::new(db)
        .retry_executions(spec.id, Some(&job_ids), true)
        .await
        .map_err(db_error)?;
    BatchSpecStore::new(db)
        .reset_auto_apply(spec.id)
        .await
        .map_err(db_error)?;
    Ok(EmptyResponse)
}

/// `enqueueBatchSpecWorkspaceExecution`: queues the given workspaces of a resolved spec, also
/// cached ones (their steps run again): new executions for workspaces without one, retries for
/// finished ones. Running ones are an error.
pub(super) async fn enqueue_workspaces(ctx: &Context<'_>, ids: &[ID]) -> Result<EmptyResponse> {
    let actor = require_write(ctx)?;
    let (spec, keys, jobs) = workspaces_of(ctx, actor, ids).await?;
    let user = spec
        .user_id
        .ok_or_else(|| invalid("The creator of the bulk change spec has been deleted."))?;
    let db = &services(ctx)?.db;
    let resolved = BatchSpecStore::new(db)
        .resolution_job(spec.id)
        .await
        .map_err(db_error)?
        .is_some_and(|j| j.state == JobState::Completed);
    if !resolved {
        return Err(invalid(
            "The bulk change spec's workspaces are not resolved.",
        ));
    }
    let mut retry = Vec::new();
    let mut fresh = Vec::new();
    for key in &keys {
        match jobs.get(key) {
            Some(j) if finished(j.state) => retry.push(j.id),
            Some(_) => {
                return Err(invalid(
                    "A workspace's execution is still queued or running.",
                ));
            }
            None => fresh.push(*key),
        }
    }
    let store = BatchSpecWorkspaceStore::new(db);
    if !retry.is_empty() {
        store
            .retry_executions(spec.id, Some(&retry), true)
            .await
            .map_err(db_error)?;
    }
    if !fresh.is_empty() {
        store
            .enqueue_workspace_executions(spec.id, &fresh, user)
            .await
            .map_err(db_error)?;
    }
    BatchSpecStore::new(db)
        .reset_auto_apply(spec.id)
        .await
        .map_err(db_error)?;
    Ok(EmptyResponse)
}

/// The failure message of a spec's executions (`None` without
/// failures).
pub(super) async fn failure_message(ctx: &Context<'_>, spec: &BatchSpec) -> Result<Option<String>> {
    let store = BatchSpecWorkspaceStore::new(&services(ctx)?.db);
    let messages = store
        .failure_messages(spec.id, MAX_FAILURES + 1)
        .await
        .map_err(db_error)?;
    if messages.is_empty() {
        return Ok(None);
    }
    let mut out = String::from("Failures:\n\n");
    for m in messages
        .iter()
        .take(usize::try_from(MAX_FAILURES).unwrap_or(10))
    {
        out.push_str("* ");
        out.push_str(m.trim());
        out.push('\n');
    }
    if i64::try_from(messages.len()).unwrap_or(i64::MAX) > MAX_FAILURES {
        let failed = store.stats(spec.id).await.map_err(db_error)?.failed;
        out.push_str(&format!("\nand {} more\n", failed - MAX_FAILURES));
    }
    Ok(Some(out))
}

// ------------------------------------------------------------------ stages

/// `BatchSpecWorkspaceStages`: a workspace execution's log entries by stage.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceStages")]
pub struct WorkspaceStages {
    /// Checkout and image pulls (`setup.*`).
    setup: Vec<ExecutionLogEntryNode>,
    /// The steps (`step.*`).
    src_exec: Vec<ExecutionLogEntryNode>,
    /// Cleanup (`teardown.*`).
    teardown: Vec<ExecutionLogEntryNode>,
}

/// The stages of a workspace's latest execution; `None` without one.
pub(super) async fn stages(
    ctx: &Context<'_>,
    d: &WorkspaceData,
) -> Result<Option<WorkspaceStages>> {
    let Some(job) = &d.job else {
        return Ok(None);
    };
    let show = viewer_can_see_logs(ctx, &d.spec).await?;
    let node = |e: &ExecutionLogEntry| {
        if show {
            ExecutionLogEntryNode::visible(e.clone())
        } else {
            ExecutionLogEntryNode::admin_only(e.clone())
        }
    };
    let with = |prefix: &str| -> Vec<ExecutionLogEntryNode> {
        job.execution_logs
            .iter()
            .filter(|e| e.key.starts_with(prefix))
            .map(node)
            .collect()
    };
    Ok(Some(WorkspaceStages {
        setup: with("setup."),
        src_exec: with("step."),
        teardown: with("teardown."),
    }))
}

// ------------------------------------------------------------------ steps

/// What the log says about step `i`.
#[derive(Default, Clone)]
struct StepLog {
    entry: Option<ExecutionLogEntry>,
    skipped: bool,
    cached: bool,
}

/// Step logs by step index; `cached` covers every step up to the one restored from the
/// cache.
fn step_logs(logs: &[ExecutionLogEntry], steps: usize) -> Vec<StepLog> {
    let mut out = vec![StepLog::default(); steps];
    let mut cached_up_to = None;
    for e in logs {
        let Some(rest) = e.key.strip_prefix("step.") else {
            continue;
        };
        let (index, suffix) = rest.split_once('.').unwrap_or((rest, ""));
        let Ok(i) = index.parse::<usize>() else {
            continue;
        };
        let Some(s) = out.get_mut(i) else {
            continue;
        };
        match suffix {
            "" => s.entry = Some(e.clone()),
            "skipped" => s.skipped = true,
            "cached" => cached_up_to = Some(cached_up_to.map_or(i, |c: usize| c.max(i))),
            _ => {}
        }
    }
    if let Some(k) = cached_up_to {
        for s in out.iter_mut().take(k + 1) {
            s.cached = true;
        }
    }
    out
}

/// `BatchSpecWorkspaceEnvironmentVariable`.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceEnvironmentVariable")]
pub struct EnvironmentVariable {
    name: String,
    /// The value as written; null for a variable passed through from the namespace's executor
    /// secrets (never shown).
    value: Option<String>,
}

/// `BatchSpecWorkspaceOutputVariable`.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceOutputVariable")]
pub struct OutputVariable {
    name: String,
    value: JsonValue,
}

/// A page of a step's output lines.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceStepOutputLineConnection")]
pub struct OutputLineConnection {
    total_count: i32,
    page_info: PageInfo,
    nodes: Vec<String>,
}

/// `BatchSpecWorkspaceStep`: one step of a workspace's execution.
pub struct WorkspaceStepNode {
    /// From 1.
    number: usize,
    run: String,
    container: String,
    if_condition: Option<String>,
    env: Vec<EnvironmentVariable>,
    /// Names of the outputs the step defines.
    output_names: Vec<String>,
    log: StepLog,
    show_output: bool,
    /// The step's result from the execution cache.
    result: Option<AfterStepResult>,
}

#[Object(name = "BatchSpecWorkspaceStep")]
impl WorkspaceStepNode {
    /// The step's number, from 1.
    async fn number(&self) -> i32 {
        i32::try_from(self.number).unwrap_or(i32::MAX)
    }

    /// The `run` script, as written.
    async fn run(&self) -> &str {
        &self.run
    }

    /// The container image.
    async fn container(&self) -> &str {
        &self.container
    }

    /// The `if:` condition, as written; null when the step always runs.
    async fn if_condition(&self) -> Option<&str> {
        self.if_condition.as_deref()
    }

    /// The step's result came from the cache.
    async fn cached_result_found(&self) -> bool {
        self.log.cached
    }

    /// The step's `if:` was false.
    async fn skipped(&self) -> bool {
        self.log.skipped
    }

    /// The step's output, a line per node (`stdout: …`/`stderr: …`); empty for viewers who
    /// may not see the spec's logs.
    async fn output_lines(
        &self,
        #[graphql(default = 500)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<OutputLineConnection> {
        let first = i64::from(first.unwrap_or(500));
        if !(0..=MAX_OUTPUT_LINES).contains(&first) {
            return Err(invalid(format!(
                "first must be between 0 and {MAX_OUTPUT_LINES}."
            )));
        }
        let lines: Vec<&str> = match (&self.log.entry, self.show_output) {
            (Some(e), true) => e.out.lines().collect(),
            _ => Vec::new(),
        };
        let start = match after.as_deref() {
            None | Some("") => 0,
            Some(c) => c
                .parse::<usize>()
                .map_err(|_| invalid(format!("Invalid cursor {c:?}.")))?,
        };
        let end = start
            .saturating_add(usize::try_from(first).unwrap_or(0))
            .min(lines.len());
        let nodes: Vec<String> = lines
            .get(start.min(lines.len())..end)
            .unwrap_or_default()
            .iter()
            .map(|l| (*l).to_owned())
            .collect();
        let has_next_page = end < lines.len();
        Ok(OutputLineConnection {
            total_count: to_int(i64::try_from(lines.len()).unwrap_or(i64::MAX)),
            page_info: PageInfo {
                has_next_page,
                end_cursor: has_next_page.then(|| end.to_string()),
            },
            nodes,
        })
    }

    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.log.entry.as_ref()?.start_time
    }

    /// When the step's container exited.
    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        let e = self.log.entry.as_ref()?;
        e.exit_code?;
        Some(e.start_time? + Duration::milliseconds(e.duration_ms.unwrap_or(0)))
    }

    async fn exit_code(&self) -> Option<i32> {
        self.log.entry.as_ref()?.exit_code
    }

    /// The step's environment as written (pass-through values hidden).
    async fn environment(&self) -> &[EnvironmentVariable] {
        &self.env
    }

    /// The outputs the step set; null until the step's result is known.
    async fn output_variables(&self) -> Option<Vec<OutputVariable>> {
        let result = self.result.as_ref()?;
        Some(
            self.output_names
                .iter()
                .filter_map(|name| {
                    let v = result.outputs.get(name)?;
                    Some(OutputVariable {
                        name: name.clone(),
                        value: JsonValue(v.to_json()),
                    })
                })
                .collect(),
        )
    }

    /// The diff stat of the workspace's changes after the step (cumulative); null until the
    /// step's result is known.
    async fn diff_stat(&self) -> Option<DiffStat> {
        let result = self.result.as_ref()?;
        let (added, deleted) = sourcegull_services::batches::diff_stat(&result.diff);
        Some(DiffStat {
            added,
            deleted,
            changed: 0,
        })
    }
}

/// The steps of a workspace in repository `repo`.
pub(super) async fn steps(
    ctx: &Context<'_>,
    d: &WorkspaceData,
    repo: &Repo,
) -> Result<Vec<WorkspaceStepNode>> {
    let Ok(parsed) = ParsedSpec::parse(&d.spec.raw_spec) else {
        return Ok(Vec::new());
    };
    let n = parsed.steps.len();
    let mut logs = match &d.job {
        Some(job) => step_logs(&job.execution_logs, n),
        None => vec![StepLog::default(); n],
    };
    if d.job.is_none() && d.ws.cached_result_found {
        // Built from the cache without an execution.
        for s in &mut logs {
            s.cached = true;
        }
    }
    let results = cached_results(ctx, d, &parsed, repo).await?;
    let show_output = viewer_can_see_logs(ctx, &d.spec).await?;
    Ok(parsed
        .steps
        .iter()
        .zip(logs)
        .enumerate()
        .map(|(i, (step, log))| WorkspaceStepNode {
            number: i + 1,
            run: step.run.clone(),
            container: step.container.clone(),
            if_condition: step.if_condition(),
            env: step
                .env
                .vars()
                .iter()
                .map(|v| EnvironmentVariable {
                    name: v.name.clone(),
                    value: v.value.clone(),
                })
                .collect(),
            output_names: step.outputs.keys().cloned().collect(),
            log,
            show_output,
            result: results.get(&i).cloned(),
        })
        .collect())
}

/// The cached result of each step of the workspace, by step index.
async fn cached_results(
    ctx: &Context<'_>,
    d: &WorkspaceData,
    parsed: &ParsedSpec,
    repo: &Repo,
) -> Result<BTreeMap<usize, AfterStepResult>> {
    let Some(user) = d.job.as_ref().map(|j| j.payload.user_id).or(d.spec.user_id) else {
        return Ok(BTreeMap::new());
    };
    let keys = step_cache_keys(parsed, &repo.name, &d.ws);
    if keys.is_empty() {
        return Ok(BTreeMap::new());
    }
    let entries = ExecutionCacheStore::new(&services(ctx)?.db)
        .peek_many(user, &keys)
        .await
        .map_err(db_error)?;
    let by_key: HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    Ok(keys
        .iter()
        .enumerate()
        .filter_map(|(i, k)| {
            let entry = CachedStep::from_json(by_key.get(k.as_str())?).ok()?;
            (entry.result.step_index == i).then_some((i, entry.result))
        })
        .collect())
}

/// The executor that ran (or runs) the workspace's latest execution (site admins only).
pub(super) async fn executor(ctx: &Context<'_>, d: &WorkspaceData) -> Result<Option<ExecutorRef>> {
    if !viewer(ctx).is_some_and(|a| a.site_admin) {
        return Ok(None);
    }
    let Some(host) = d
        .job
        .as_ref()
        .map(|j| j.worker_hostname.as_str())
        .filter(|h| !h.is_empty())
    else {
        return Ok(None);
    };
    Ok(services(ctx)?
        .db
        .executors()
        .list(None)
        .await
        .map_err(db_error)?
        .into_iter()
        .find(|e| e.hostname == host)
        .map(ExecutorRef))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str) -> ExecutionLogEntry {
        ExecutionLogEntry {
            key: key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn step_log_keys() {
        let logs = [
            entry("setup.git.init"),
            entry("step.1.cached"),
            entry("step.2.skipped"),
            entry("step.3"),
            entry("step.9"),
            entry("step.x"),
            entry("teardown.fs"),
        ];
        let s = step_logs(&logs, 4);
        assert_eq!(
            s.iter().map(|s| s.cached).collect::<Vec<_>>(),
            [true, true, false, false]
        );
        assert!(s[2].skipped && !s[3].skipped);
        assert_eq!(s[3].entry.as_ref().unwrap().key, "step.3");
        assert!(s[0].entry.is_none());
    }
}
