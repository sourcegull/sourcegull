//! Choosing and queueing a commit's index jobs.
//!
//! The jobs of a commit come from the first of: a configuration passed explicitly (GraphQL
//! `queueAutoIndexJobsForRepo(configuration:)`), the repository's configuration saved in the UI,
//! `sourcegull.yaml` at the commit's root, and inference. A configuration that does not parse yields no
//! jobs rather than falling through (an explicit configuration always wins, even when broken).
//!
//! Queueing skips a commit that an upload not made by auto-indexing covers or whose jobs were
//! queued already, and each (root, indexer) whose latest job is not marked for reindexing;
//! `force` (manual queueing) skips both checks.

use sourcegull_codeintel_scip::store::{IndexDockerStep, IndexJob, IndexJobStore, normalize_root};
use std::sync::Arc;

use crate::store::AutoIndexStore;
use sourcegull_config::SiteConfigHandle;
use sourcegull_db::{Db, DbError};
use sourcegull_rpc::RepoClients;
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_services::git::{GitCallError, GitReader, TreeOptions};
use sourcegull_types::metric_names;
use sourcegull_types::{RepoId, UserId};

use crate::config::{CONFIG_FILES, IndexConfiguration, IndexJobSpec};
use crate::indexers::IndexerMap;
use crate::inference::{InferenceError, InferenceResult, Limits, RepoFiles, infer};

/// Inferred configurations with more jobs than this are not queued automatically.
pub const MAX_INFERRED_JOBS: usize = 50;

/// Errors of auto-indexing operations.
#[derive(Debug, thiserror::Error)]
pub enum AutoIndexError {
    #[error(transparent)]
    Inference(#[from] InferenceError),
    #[error("git: {0}")]
    Git(#[from] GitCallError),
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("{0}")]
    Invalid(String),
}

impl From<sourcegull_codeintel_policies::PolicyError> for AutoIndexError {
    fn from(e: sourcegull_codeintel_policies::PolicyError) -> Self {
        match e {
            sourcegull_codeintel_policies::PolicyError::Git(e) => Self::Git(e),
            sourcegull_codeintel_policies::PolicyError::Db(e) => Self::Db(e),
        }
    }
}

/// A repository by id and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub id: RepoId,
    pub name: String,
}

/// Where a commit's jobs came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobSource {
    Explicit,
    Database,
    RepositoryFile,
    Inferred,
}

/// Auto-indexing over the database and the repository nodes.
#[derive(Clone)]
pub struct AutoIndexer {
    db: Db,
    clients: Arc<RepoClients>,
    config: SiteConfigHandle,
    limits: Limits,
}

impl std::fmt::Debug for AutoIndexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoIndexer").finish_non_exhaustive()
    }
}

impl AutoIndexer {
    pub fn new(db: Db, clients: Arc<RepoClients>, config: SiteConfigHandle) -> Self {
        Self {
            db,
            clients,
            config,
            limits: Limits::default(),
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn clients(&self) -> &Arc<RepoClients> {
        &self.clients
    }

    /// Whether `codeIntelAutoIndexing.enabled` is set.
    pub fn enabled(&self) -> bool {
        self.config.load().value.code_intel_auto_indexing_enabled
    }

    /// The current site config.
    pub fn site_config(&self) -> sourcegull_config::SiteConfig {
        self.config.load().value.clone()
    }

    fn reader(&self, repo: &RepoRef) -> GitReader {
        GitReader::from_parts(self.clients.clone(), repo.id, &repo.name)
    }

    /// The commit `rev` names (`""` or `HEAD`: the default branch).
    pub async fn resolve(&self, repo: &RepoRef, rev: &str) -> Result<String, AutoIndexError> {
        let rev = if rev == "HEAD" { "" } else { rev };
        Ok(self.reader(repo).resolve_revision(rev).await?)
    }

    /// Infers jobs at `commit` with the site's inference script, or `script` instead when
    /// given (to try a script before saving it).
    pub async fn infer(
        &self,
        repo: &RepoRef,
        commit: &str,
        script: Option<String>,
    ) -> Result<InferenceResult, AutoIndexError> {
        let script = match script.filter(|s| !s.trim().is_empty()) {
            Some(s) => s,
            None => AutoIndexStore::new(&self.db).inference_script().await?,
        };
        let indexers = IndexerMap::new(
            self.config
                .load()
                .value
                .code_intel_auto_indexing_indexer_map
                .clone(),
        );
        let mut files = GitFiles {
            reader: self.reader(repo),
            commit: commit.to_owned(),
            handle: tokio::runtime::Handle::current(),
        };
        let limits = self.limits.clone();
        let result = tokio::task::spawn_blocking(move || {
            infer(&mut files, Some(&script), &indexers, &limits)
        })
        .await
        .map_err(|e| AutoIndexError::Invalid(format!("inference task failed: {e}")))?;
        Ok(result?)
    }

    /// The jobs for `commit` and where they came from (see the module docs). With
    /// `bypass_limit`, inferred configurations of any size are returned.
    pub async fn jobs_for(
        &self,
        repo: &RepoRef,
        commit: &str,
        configuration: Option<&str>,
        bypass_limit: bool,
    ) -> Result<(Vec<IndexJobSpec>, JobSource), AutoIndexError> {
        if let Some(c) = configuration.filter(|c| !c.trim().is_empty()) {
            return Ok((
                parsed(repo, IndexConfiguration::from_json(c)),
                JobSource::Explicit,
            ));
        }
        if let Some(row) = AutoIndexStore::new(&self.db)
            .index_configuration(repo.id)
            .await?
        {
            let c = IndexConfiguration::from_json(&row.data);
            return Ok((parsed(repo, c), JobSource::Database));
        }
        let reader = self.reader(repo);
        for file in CONFIG_FILES {
            match reader.read_file(commit, file, 1 << 20).await {
                Ok(f) => {
                    let text = String::from_utf8_lossy(&f.content);
                    let c = IndexConfiguration::from_yaml(&text);
                    return Ok((parsed(repo, c), JobSource::RepositoryFile));
                }
                Err(e) if e.is_not_found() || matches!(e, GitCallError::WrongEntryType(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let result = self.infer(repo, commit, None).await?;
        let mut jobs = result.jobs;
        if !bypass_limit && jobs.len() > MAX_INFERRED_JOBS {
            tracing::info!(repo = %repo.name, jobs = jobs.len(),
                "too many inferred index jobs; queueing none");
            jobs.clear();
        }
        Ok((jobs, JobSource::Inferred))
    }

    /// Queues the jobs of `commit` (see the module docs) and returns the new job ids.
    pub async fn queue(
        &self,
        repo: &RepoRef,
        commit: &str,
        configuration: Option<&str>,
        force: bool,
        enqueuer: Option<UserId>,
    ) -> Result<Vec<i64>, AutoIndexError> {
        let store = IndexJobStore::new(&self.db);
        if !force && store.is_queued(repo.id, commit).await? {
            return Ok(Vec::new());
        }
        let (specs, _) = self.jobs_for(repo, commit, configuration, force).await?;
        let mut jobs = Vec::new();
        for spec in specs {
            let Some(job) = to_job(repo, commit, spec, enqueuer) else {
                continue;
            };
            if !force
                && store
                    .is_queued_root_indexer(repo.id, commit, &job.root, &job.indexer)
                    .await?
            {
                continue;
            }
            // One job per root and indexer even if a configuration lists one twice.
            if jobs
                .iter()
                .any(|j: &IndexJob| j.root == job.root && j.indexer == job.indexer)
            {
                continue;
            }
            jobs.push(job);
        }
        let ids = store.enqueue_many(&jobs).await?;
        if !ids.is_empty() {
            metrics::counter!(JOBS_QUEUED_METRIC).increment(ids.len() as u64);
            tracing::info!(repo = %repo.name, commit, jobs = ids.len(), "queued index jobs");
        }
        Ok(ids)
    }
}

/// Counter of index jobs queued.
pub const JOBS_QUEUED_METRIC: &str = metric_names::AUTOINDEX_JOBS_QUEUED_TOTAL;

fn parsed(
    repo: &RepoRef,
    c: Result<IndexConfiguration, crate::config::ConfigError>,
) -> Vec<IndexJobSpec> {
    match c {
        Ok(c) => c.into_jobs(),
        Err(e) => {
            tracing::warn!(repo = %repo.name, error = %e, "invalid index configuration");
            Vec::new()
        }
    }
}

/// The queue row for a job spec; `None` (logged) for roots outside the repository.
pub fn to_job(
    repo: &RepoRef,
    commit: &str,
    spec: IndexJobSpec,
    enqueuer: Option<UserId>,
) -> Option<IndexJob> {
    let Some(root) = normalize_root(&spec.root) else {
        tracing::warn!(repo = %repo.name, root = %spec.root, "index job root outside the repository");
        return None;
    };
    let mut docker_steps = Vec::with_capacity(spec.steps.len());
    for s in spec.steps {
        let Some(step_root) = normalize_root(&s.root) else {
            tracing::warn!(repo = %repo.name, root = %s.root, "step root outside the repository");
            return None;
        };
        docker_steps.push(IndexDockerStep {
            root: step_root.trim_end_matches('/').to_owned(),
            image: s.image,
            commands: s.commands,
        });
    }
    Some(IndexJob {
        repo_id: repo.id,
        commit: commit.to_owned(),
        root,
        indexer: spec.indexer.trim().to_owned(),
        indexer_args: spec.indexer_args,
        outfile: spec.outfile,
        docker_steps,
        local_steps: spec.local_steps,
        requested_env_vars: spec.requested_env_vars,
        should_reindex: false,
        enqueuer,
        associated_upload_id: None,
    })
}

/// The jobs of a configuration as specs again (for showing queued jobs' configuration).
pub fn to_spec(job: &IndexJob) -> IndexJobSpec {
    IndexJobSpec {
        steps: job
            .docker_steps
            .iter()
            .map(|s| crate::config::DockerStep {
                root: s.root.clone(),
                image: s.image.clone(),
                commands: s.commands.clone(),
            })
            .collect(),
        local_steps: job.local_steps.clone(),
        root: job.root.trim_end_matches('/').to_owned(),
        indexer: job.indexer.clone(),
        indexer_args: job.indexer_args.clone(),
        outfile: job.outfile.clone(),
        requested_env_vars: job.requested_env_vars.clone(),
    }
}

/// The repository at a commit through its repository node, for inference on a blocking
/// thread (each read waits on the runtime).
struct GitFiles {
    reader: GitReader,
    commit: String,
    handle: tokio::runtime::Handle,
}

impl RepoFiles for GitFiles {
    fn paths(&mut self) -> Result<Vec<String>, InferenceError> {
        let opts = TreeOptions {
            recursive: true,
            include_sizes: false,
            limit: 0,
        };
        let (entries, truncated) = self
            .handle
            .block_on(self.reader.list_tree(&self.commit, "", &opts))
            .map_err(|e| InferenceError::Repository(format!("listing files: {e}")))?;
        if truncated {
            tracing::info!(repo = %self.reader.repo_name(), "inference sees a truncated file list");
        }
        Ok(entries
            .into_iter()
            .filter(|e| {
                matches!(
                    pb::EntryKind::try_from(e.kind),
                    Ok(pb::EntryKind::File | pb::EntryKind::Executable)
                )
            })
            .map(|e| e.path)
            .collect())
    }

    fn contents(
        &mut self,
        paths: &[String],
        max_size: u64,
    ) -> Result<Vec<(String, String)>, InferenceError> {
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let r = self.handle.block_on(self.reader.read_file(
                &self.commit,
                p,
                max_size.saturating_add(1),
            ));
            match r {
                Ok(f) => {
                    if f.truncated || f.size > max_size {
                        return Err(InferenceError::Limit(format!(
                            "requested content for a file larger than {max_size} bytes ({p})"
                        )));
                    }
                    out.push((p.clone(), String::from_utf8_lossy(&f.content).into_owned()));
                }
                Err(e) if e.is_not_found() => {}
                Err(e) => {
                    return Err(InferenceError::Repository(format!("reading {p}: {e}")));
                }
            }
        }
        Ok(out)
    }
}

/// Recognizer output as a JSON configuration (what `inferredConfiguration` shows).
pub fn configuration_json(jobs: &[IndexJobSpec]) -> String {
    IndexConfiguration {
        index_jobs: jobs.to_vec(),
    }
    .to_json()
}

/// The comparison key of a (root, indexer) pair: the same project in explicit and inferred
/// configurations has the same key.
pub fn comparison_key(root: &str, indexer: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(root.trim_end_matches('/').as_bytes());
    h.update(b"\0");
    h.update(crate::config::indexer_name(indexer).as_bytes());
    hex::encode(h.finalize())
}
