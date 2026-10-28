//! Auto-indexing over GraphQL: the site's inference
//! script (`codeIntelligenceInferenceScript`, `updateCodeIntelligenceInferenceScript`), a
//! repository's index configuration (`Repository.indexConfiguration`,
//! `updateRepositoryIndexConfiguration`), and inferring or queueing a commit's jobs
//! (`inferAutoIndexJobsForRepo`, `queueAutoIndexJobsForRepo`). An index job's configuration
//! and execution logs are `IndexSteps` (`PreciseIndex.steps`).
//!
//! All of it but reading a repository's configuration is for site admins,
//! and everything but the inference script needs `codeIntelAutoIndexing.enabled`. Execution
//! log output is shown to site admins only.

use std::collections::BTreeMap;

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use sourcegull_autoindex::config::IndexConfiguration;
use sourcegull_autoindex::service::{comparison_key, configuration_json};
use sourcegull_autoindex::store::AutoIndexStore;
use sourcegull_autoindex::{
    AutoIndexError, AutoIndexer, IndexJobSpec, IndexerMap, InferenceError, Limits, RepoRef,
};
use sourcegull_codeintel_scip::store::{IndexJob, IndexJobStore};
use sourcegull_pgqueue::ExecutionLogEntry;

use crate::context::{codes, db_error, error, require_site_admin, services};
use crate::precise_indexes::{CodeIntelIndexer, PreciseIndexNode};
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::execution::ExecutionLogEntryNode;
use sourcegull_graphql_core::ids::parse_repository_id;
use sourcegull_types::Repo;

/// The error when auto-indexing is off.
const DISABLED: &str = "precise code intelligence auto-indexing is not enabled";

/// The auto-indexing service for this request.
pub(crate) fn auto_indexer(ctx: &Context<'_>) -> Result<AutoIndexer> {
    let svc = services(ctx)?;
    Ok(AutoIndexer::new(
        svc.db.clone(),
        svc.repo_clients.clone(),
        svc.config.clone(),
    ))
}

fn require_enabled(indexer: &AutoIndexer) -> Result<()> {
    if indexer.enabled() {
        Ok(())
    } else {
        Err(error(codes::FORBIDDEN, DISABLED))
    }
}

/// Maps an auto-indexing error to a GraphQL error.
pub(crate) fn autoindex_error(e: AutoIndexError) -> async_graphql::Error {
    match e {
        AutoIndexError::Git(e) => sourcegull_graphql_core::git::git_error(e),
        AutoIndexError::Db(e) => db_error(e),
        AutoIndexError::Inference(InferenceError::Limit(m)) => {
            error(codes::TOO_LARGE, format!("inference limit: {m}"))
        }
        AutoIndexError::Inference(e @ InferenceError::Script(_)) => {
            error(codes::INVALID_INPUT, e.to_string())
        }
        AutoIndexError::Inference(e @ InferenceError::Repository(_)) => {
            error(codes::UNAVAILABLE, e.to_string())
        }
        AutoIndexError::Invalid(m) => error(codes::INVALID_INPUT, m),
    }
}

/// The repository behind a `Repository` ID, for site admins.
async fn admin_repo(ctx: &Context<'_>, id: &ID) -> Result<RepoRef> {
    require_site_admin(ctx)?;
    let not_found = || error(codes::NOT_FOUND, "Repository not found.");
    let id = parse_repository_id(id).map_err(|_| not_found())?;
    let repo = services(ctx)?
        .db
        .repos()
        .get_authorized(id, &sourcegull_types::RepoAccess::All)
        .await
        .map_err(db_error)?
        .ok_or_else(not_found)?;
    Ok(RepoRef {
        id,
        name: repo.name,
    })
}

// --- execution logs and steps ---

/// `PreIndexStep`: a step run in its own container before the indexer.
#[derive(SimpleObject, Debug, Clone)]
pub struct PreIndexStep {
    /// The working directory, relative to the repository root.
    root: String,
    image: String,
    commands: Vec<String>,
    log_entry: Option<ExecutionLogEntryNode>,
}

/// `IndexStep`: the indexer.
#[derive(SimpleObject, Debug, Clone)]
pub struct IndexStep {
    /// Commands run in the indexer's container before the indexer.
    commands: Vec<String>,
    indexer_args: Vec<String>,
    outfile: Option<String>,
    /// Executor secrets the job asks for (names only, not resolved).
    requested_env_vars: Option<Vec<String>>,
    log_entry: Option<ExecutionLogEntryNode>,
}

/// `IndexSteps`: an index job's configuration and, once it ran, its execution logs.
#[derive(SimpleObject, Debug, Clone)]
pub struct IndexSteps {
    /// Workspace setup (checkout, image pulls).
    setup: Vec<ExecutionLogEntryNode>,
    pre_index: Vec<PreIndexStep>,
    index: IndexStep,
    /// The upload of the index; null until it ran.
    upload: Option<ExecutionLogEntryNode>,
    teardown: Vec<ExecutionLogEntryNode>,
}

impl IndexSteps {
    /// The steps of `spec` with the log entries of `logs` (the executor's keys:
    /// `setup.*`, `step.docker.pre-index.<i>`, `step.docker.indexer`, `step.upload`,
    /// `teardown.*`; also their `kubernetes` and `src` variants).
    pub fn new(spec: &IndexJobSpec, logs: &[ExecutionLogEntry]) -> Self {
        let find = |pred: &dyn Fn(&str) -> bool| {
            logs.iter()
                .find(|e| pred(&e.key))
                .cloned()
                .map(ExecutionLogEntryNode::admin_only)
        };
        let runtime_step = |key: &str, name: &str| {
            ["docker", "kubernetes"].iter().any(|rt| {
                key.strip_prefix(&format!("step.{rt}."))
                    .is_some_and(|k| k == name)
            })
        };
        let pre_index = spec
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| PreIndexStep {
                root: s.root.clone(),
                image: s.image.clone(),
                commands: s.commands.clone(),
                log_entry: find(&|k| {
                    runtime_step(k, &format!("pre-index.{i}")) || runtime_step(k, &i.to_string())
                }),
            })
            .collect();
        let n = spec.steps.len().to_string();
        Self {
            setup: logs
                .iter()
                .filter(|e| e.key.starts_with("setup."))
                .cloned()
                .map(ExecutionLogEntryNode::admin_only)
                .collect(),
            pre_index,
            index: IndexStep {
                commands: spec.local_steps.clone(),
                indexer_args: spec.indexer_args.clone(),
                outfile: Some(spec.outfile.clone()).filter(|o| !o.is_empty()),
                requested_env_vars: Some(spec.requested_env_vars.clone()).filter(|v| !v.is_empty()),
                log_entry: find(&|k| runtime_step(k, "indexer") || runtime_step(k, &n)),
            },
            upload: find(&|k| {
                k == "step.upload"
                    || ["docker", "kubernetes", "src"]
                        .iter()
                        .any(|rt| k.starts_with(&format!("step.{rt}.upload")))
                    || k == "step.src.0"
            }),
            teardown: logs
                .iter()
                .filter(|e| e.key.starts_with("teardown."))
                .cloned()
                .map(ExecutionLogEntryNode::admin_only)
                .collect(),
        }
    }

    /// The steps of a queued job.
    pub fn of_job(job: &IndexJob, logs: &[ExecutionLogEntry]) -> Self {
        Self::new(&sourcegull_autoindex::service::to_spec(job), logs)
    }
}

// --- job descriptions and configuration ---

/// `AutoIndexJobDescription`: a job a configuration or inference describes.
#[derive(SimpleObject, Debug, Clone)]
pub struct AutoIndexJobDescription {
    /// The project root (`""` for the repository root).
    root: String,
    indexer: Option<CodeIntelIndexer>,
    /// Equal for the same root and indexer in explicit and inferred configurations.
    comparison_key: String,
    steps: IndexSteps,
}

fn descriptions(jobs: &[IndexJobSpec]) -> Vec<AutoIndexJobDescription> {
    jobs.iter()
        .map(|j| AutoIndexJobDescription {
            root: j.root.clone(),
            indexer: CodeIntelIndexer::of(&j.indexer),
            comparison_key: comparison_key(&j.root, &j.indexer),
            steps: IndexSteps::new(j, &[]),
        })
        .collect()
}

fn parse_descriptions(json: &str) -> Result<Vec<AutoIndexJobDescription>> {
    let c = IndexConfiguration::from_json(json)
        .map_err(|e| error(codes::INVALID_INPUT, e.to_string()))?;
    Ok(descriptions(&c.index_jobs))
}

/// `InferAutoIndexJobsResult`.
#[derive(SimpleObject, Debug, Clone)]
pub struct InferAutoIndexJobsResult {
    jobs: Vec<AutoIndexJobDescription>,
    /// What the recognizers printed.
    inference_output: String,
}

/// `InferredConfiguration`: what inference gives at the default branch's head.
#[derive(Debug, Clone, Default)]
pub struct InferredConfiguration {
    configuration: String,
    jobs: Vec<IndexJobSpec>,
    limit_error: Option<String>,
}

#[Object]
impl InferredConfiguration {
    /// The inferred jobs as a JSON configuration.
    async fn configuration(&self) -> &str {
        &self.configuration
    }

    async fn parsed_configuration(&self) -> Option<Vec<AutoIndexJobDescription>> {
        Some(descriptions(&self.jobs))
    }

    /// Set when inference hit a limit (too many files with content, a slow or large script).
    async fn limit_error(&self) -> Option<&str> {
        self.limit_error.as_deref()
    }
}

/// `IndexConfiguration`: a repository's saved index configuration and the inferred one.
pub struct IndexConfigurationNode {
    repo: RepoRef,
}

#[Object(name = "IndexConfiguration")]
impl IndexConfigurationNode {
    /// The saved JSON configuration, if any.
    async fn configuration(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(AutoIndexStore::new(&services(ctx)?.db)
            .index_configuration(self.repo.id)
            .await
            .map_err(db_error)?
            .map(|r| r.data)
            .filter(|d| !d.is_empty()))
    }

    /// The saved configuration's jobs.
    async fn parsed_configuration(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<Vec<AutoIndexJobDescription>>> {
        match self.configuration(ctx).await? {
            Some(c) => parse_descriptions(&c).map(Some),
            None => Ok(None),
        }
    }

    /// The jobs inference gives at the head of the default branch.
    async fn inferred_configuration(&self, ctx: &Context<'_>) -> Result<InferredConfiguration> {
        let indexer = auto_indexer(ctx)?;
        let commit = indexer
            .resolve(&self.repo, "")
            .await
            .map_err(autoindex_error)?;
        match indexer.infer(&self.repo, &commit, None).await {
            Ok(r) => Ok(InferredConfiguration {
                configuration: configuration_json(&r.jobs),
                jobs: r.jobs,
                limit_error: None,
            }),
            Err(AutoIndexError::Inference(InferenceError::Limit(m))) => Ok(InferredConfiguration {
                configuration: configuration_json(&[]),
                jobs: Vec::new(),
                limit_error: Some(format!("inference limit: {m}")),
            }),
            Err(e) => Err(autoindex_error(e)),
        }
    }
}

/// `Repository.indexConfiguration`.
pub async fn index_configuration(
    ctx: &Context<'_>,
    repo: &Repo,
) -> Result<Option<IndexConfigurationNode>> {
    require_enabled(&auto_indexer(ctx)?)?;
    Ok(Some(IndexConfigurationNode {
        repo: RepoRef {
            id: repo.id,
            name: repo.name.clone(),
        },
    }))
}

#[derive(Default)]
pub struct AutoIndexQuery;

#[Object]
impl AutoIndexQuery {
    /// The site's inference script (site admins only); `""` when the built-in recognizers
    /// run unchanged.
    async fn code_intelligence_inference_script(&self, ctx: &Context<'_>) -> Result<String> {
        require_site_admin(ctx)?;
        AutoIndexStore::new(&services(ctx)?.db)
            .inference_script()
            .await
            .map_err(db_error)
    }

    /// The jobs inference gives for a repository at `rev` (default: HEAD), with `script` in
    /// place of the site's inference script if given; nothing is queued (site admins only).
    async fn infer_auto_index_jobs_for_repo(
        &self,
        ctx: &Context<'_>,
        repository: ID,
        rev: Option<String>,
        script: Option<String>,
    ) -> Result<InferAutoIndexJobsResult> {
        let repo = admin_repo(ctx, &repository).await?;
        let indexer = auto_indexer(ctx)?;
        require_enabled(&indexer)?;
        let commit = indexer
            .resolve(&repo, rev.as_deref().unwrap_or(""))
            .await
            .map_err(autoindex_error)?;
        let r = indexer
            .infer(&repo, &commit, script)
            .await
            .map_err(autoindex_error)?;
        Ok(InferAutoIndexJobsResult {
            jobs: descriptions(&r.jobs),
            inference_output: r.output,
        })
    }
}

#[derive(Default)]
pub struct AutoIndexMutation;

#[Object]
impl AutoIndexMutation {
    /// Replaces the site's inference script (site admins only); empty restores the built-in
    /// recognizers. The script must run and return recognizers.
    async fn update_code_intelligence_inference_script(
        &self,
        ctx: &Context<'_>,
        script: String,
    ) -> Result<EmptyResponse> {
        require_site_admin(ctx)?;
        if !script.trim().is_empty() {
            let mut empty: BTreeMap<String, String> = BTreeMap::new();
            let map = IndexerMap::new(
                services(ctx)?
                    .config
                    .load()
                    .value
                    .code_intel_auto_indexing_indexer_map
                    .clone(),
            );
            let s = script.clone();
            tokio::task::spawn_blocking(move || {
                sourcegull_autoindex::infer(&mut empty, Some(&s), &map, &Limits::default())
            })
            .await
            .map_err(|e| error(codes::INTERNAL, e.to_string()))?
            .map_err(|e| autoindex_error(e.into()))?;
        }
        AutoIndexStore::new(&services(ctx)?.db)
            .set_inference_script(&script)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Saves a repository's index configuration (JSON `{"index_jobs": [...]}`; empty removes
    /// it). It takes precedence over `sourcegull.yaml` and inference (site admins only).
    async fn update_repository_index_configuration(
        &self,
        ctx: &Context<'_>,
        repository: ID,
        configuration: String,
    ) -> Result<EmptyResponse> {
        let repo = admin_repo(ctx, &repository).await?;
        require_enabled(&auto_indexer(ctx)?)?;
        IndexConfiguration::from_json(&configuration)
            .map_err(|e| error(codes::INVALID_INPUT, e.to_string()))?;
        AutoIndexStore::new(&services(ctx)?.db)
            .set_index_configuration(repo.id, &configuration)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Queues index jobs for a repository at `rev` (default: HEAD) from `configuration` if
    /// given, else the saved configuration, `sourcegull.yaml` or inference,
    /// whether or not jobs for the commit exist already (site admins only).
    async fn queue_auto_index_jobs_for_repo(
        &self,
        ctx: &Context<'_>,
        repository: ID,
        rev: Option<String>,
        configuration: Option<String>,
    ) -> Result<Vec<PreciseIndexNode>> {
        let repo = admin_repo(ctx, &repository).await?;
        let actor = require_site_admin(ctx)?;
        let indexer = auto_indexer(ctx)?;
        require_enabled(&indexer)?;
        let commit = indexer
            .resolve(&repo, rev.as_deref().unwrap_or(""))
            .await
            .map_err(autoindex_error)?;
        let ids = indexer
            .queue(
                &repo,
                &commit,
                configuration.as_deref(),
                true,
                Some(actor.user_id),
            )
            .await
            .map_err(autoindex_error)?;
        let rows = IndexJobStore::new(&services(ctx)?.db)
            .get_many(&ids)
            .await
            .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .map(|j| PreciseIndexNode {
                upload: None,
                job: Some(j),
            })
            .collect())
    }
}
