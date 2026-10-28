//! Running a watch's query as its owner, and the trigger job worker over `cm_trigger_jobs`.
//!
//! A watch is evaluated with its owner's repository permissions: a user-owned
//! watch as that user; an org-owned one as the member who last saved, enabled or disabled it,
//! while they remain a member of the org (the org itself has no permissions). Another member
//! who changes the query or the actions takes the watch over with their own permissions. `context:` is resolved as the owner sees
//! contexts.
//!
//! A trigger job searches each of its commit ranges (`to ^from` on one repository) with each
//! part of the query that watches that repository and revision, keeps up to
//! [`MAX_RESULTS`] commits, stores them, and queues the watch's enabled actions when it
//! found any: with encryption keys, each HTTP action's rendered message as a channel job on
//! the outbound webhook queue, and every other action as an action job
//! ([`MonitorStore::finish_trigger_job`]). A part whose filter has `repo:has.file()`,
//! `has.path()`, `has.content()` or `has.commit.after()` searches a range only when the range's new head passes them
//! (evaluated by [`filter_repos`], as in a search). Unreachable repo nodes fail the job,
//! which pgqueue retries with backoff.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sourcegull_authn::{Actor, AuthVia, PermissionSet};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, DbError, Namespace};
use sourcegull_pgqueue::{
    Handler, JobContext, JobError, Queue, Record, Resetter, Worker, WorkerOptions,
};
use sourcegull_query::PatternType;
use sourcegull_search::stream::{Event, SkippedReason};
use sourcegull_search::{ContextScope, RepoRevs, YesNoOnly, substitute_context};
use sourcegull_search_client::repo_predicates::filter_repos;
use sourcegull_search_client::search_repos;
use sourcegull_search_web::{ResolvedContext, SearchBackend, resolve_context};
use sourcegull_types::UserId;

use crate::actions::channel_message;
use crate::index::{CompiledMonitor, MAX_RESULTS};
use crate::render::Notification;
use crate::results::MonitorResult;
use crate::store::{ChannelAction, Monitor, MonitorStore, TriggerJob};

/// Why a watch could not be evaluated.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error(transparent)]
    Db(#[from] DbError),
    /// The query or its context no longer works for the owner.
    #[error("{0}")]
    Invalid(String),
}

/// The user a watch runs as, or `None` when there is none any more.
pub async fn owner_actor(db: &Db, monitor: &Monitor) -> Result<Option<Actor>, DbError> {
    let user = match monitor.namespace {
        Namespace::User(u) => u,
        Namespace::Org(o) => {
            let Some(editor) = monitor.updated_by.or(monitor.created_by) else {
                return Ok(None);
            };
            if !db.orgs().is_member(o, editor).await? {
                return Ok(None);
            }
            editor
        }
    };
    user_actor(db, user).await
}

/// `user` as an actor with their repository permissions, or `None` when they no longer exist.
pub async fn user_actor(db: &Db, user: UserId) -> Result<Option<Actor>, DbError> {
    let Some(u) = db.users().get_by_id(user).await? else {
        return Ok(None);
    };
    Ok(Some(Actor {
        user_id: u.id,
        username: u.username,
        site_admin: u.site_admin,
        via: AuthVia::Session,
        permissions: PermissionSet::default(),
    }))
}

/// Compiles `query` for watch `id` as `actor` sees repositories and contexts.
pub async fn compile_as(
    backend: &SearchBackend,
    id: i64,
    query: &str,
    actor: &Actor,
) -> Result<CompiledMonitor, EvalError> {
    let access = backend.authz.access(Some(actor)).await?;
    let pattern_type = PatternType::default();
    let mut query = query.to_owned();
    let mut scope = ContextScope::Global;
    match resolve_context(&backend.db, &query, pattern_type, Some(actor.user_id)).await? {
        Some(ResolvedContext::Global) => {}
        Some(ResolvedContext::Query(q)) => query = substitute_context(&query, pattern_type, &q),
        Some(ResolvedContext::Listed(s)) => scope = s,
        None => {
            return Err(EvalError::Invalid(
                "The query's search context does not exist or the owner may not use it.".to_owned(),
            ));
        }
    }
    CompiledMonitor::compile(id, &query, &scope, access, Utc::now()).map_err(EvalError::Invalid)
}

/// Runs trigger jobs.
#[derive(Debug, Clone)]
pub struct TriggerHandler {
    backend: SearchBackend,
    store: MonitorStore,
}

impl TriggerHandler {
    /// A handler without encryption keys: every action is queued as an action job.
    pub fn new(backend: SearchBackend) -> Self {
        let store = MonitorStore::new(&backend.db);
        Self { backend, store }
    }

    /// Sets the keyring that reads the actions' URLs and encrypts the channel jobs queued for
    /// HTTP actions.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Option<Arc<Keyring>>) -> Self {
        self.store = self.store.with_keyring(keyring);
        self
    }

    /// The rendered messages of the watch's enabled HTTP actions for `results`; none without
    /// encryption keys or results.
    pub async fn channel_actions(
        &self,
        job: &TriggerJob,
        results: &[MonitorResult],
    ) -> Result<Vec<ChannelAction>, JobError> {
        if results.is_empty() || !self.store.has_keyring() {
            return Ok(Vec::new());
        }
        let Some(monitor) = self
            .store
            .get(job.monitor_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(Vec::new());
        };
        let actions = self
            .store
            .actions(monitor.id)
            .await
            .map_err(JobError::retry)?;
        let external_url = self
            .backend
            .config
            .load()
            .value
            .external_url
            .clone()
            .unwrap_or_default();
        let n = Notification {
            monitor_id: monitor.id,
            description: &monitor.description,
            query: &job.query,
            results,
            external_url: &external_url,
            test: false,
        };
        let mut out = Vec::new();
        for action in actions.iter().filter(|a| a.spec.enabled) {
            if let Some((channel, body)) = channel_message(&action.spec, &n) {
                out.push(ChannelAction {
                    action_id: action.id,
                    channel,
                    target: action.spec.url.clone(),
                    body: serde_json::to_vec(&body).map_err(JobError::fatal)?,
                });
            }
        }
        Ok(out)
    }

    /// Searches a trigger job's ranges and returns what it found.
    pub async fn search(&self, job: &TriggerJob) -> Result<Vec<MonitorResult>, JobError> {
        let Some(monitor) = self
            .store
            .get(job.monitor_id)
            .await
            .map_err(JobError::retry)?
        else {
            return Ok(Vec::new());
        };
        let Some(actor) = owner_actor(&self.backend.db, &monitor)
            .await
            .map_err(JobError::retry)?
        else {
            return Err(JobError::fatal(
                "The monitor's owner no longer exists or is no longer a member of the organization.",
            ));
        };
        let compiled = match compile_as(&self.backend, monitor.id, &job.query, &actor).await {
            Ok(c) => c,
            Err(EvalError::Db(e)) => return Err(JobError::retry(e)),
            Err(EvalError::Invalid(e)) => return Err(JobError::fatal(e)),
        };
        let catalog = self
            .backend
            .catalog
            .get(&self.backend.db)
            .await
            .map_err(JobError::retry)?;
        let mut results = Vec::new();
        let mut seen = HashSet::new();
        for range in &job.ranges {
            // Gone, blocked, or (through the matchers) no longer readable by the owner.
            let Some(repo) = catalog.get(range.repo_id) else {
                continue;
            };
            for (i, unit) in compiled.units.iter().enumerate() {
                let remaining = MAX_RESULTS.saturating_sub(results.len());
                if remaining == 0 {
                    break;
                }
                let Some(revs) = compiled.unit_revs(i, repo) else {
                    continue;
                };
                if !revs.contains(&range.rev) {
                    continue;
                }
                if !unit.predicates.is_empty() {
                    let head = RepoRevs {
                        id: repo.id,
                        name: repo.name.clone(),
                        revs: vec![range.to.clone()],
                        stars: repo.stars,
                    };
                    let f = filter_repos(
                        self.backend.repo_clients.clone(),
                        vec![head],
                        &unit.predicates,
                        YesNoOnly::Yes,
                        None,
                    )
                    .await;
                    if let Some(s) = f.skipped.iter().find(|s| retryable(s.reason)) {
                        return Err(JobError::retry(format!(
                            "checking {}: {}",
                            range.repo, s.message
                        )));
                    }
                    if f.repos.is_empty() {
                        continue;
                    }
                }
                let mut revs = vec![range.to.clone()];
                revs.extend(range.from.as_ref().map(|f| format!("^{f}")));
                let target = RepoRevs {
                    id: repo.id,
                    name: repo.name.clone(),
                    revs,
                    stars: repo.stars,
                };
                let mut search = unit.search.clone();
                search.limit = remaining;
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(16);
                let run = tokio::spawn(search_repos(
                    self.backend.repo_clients.clone(),
                    vec![target],
                    search,
                    tx,
                ));
                while let Some(event) = rx.recv().await {
                    if let Event::Matches(ms) = event {
                        for m in ms.into_iter().filter_map(MonitorResult::from_match) {
                            if seen.insert((m.repository_id, m.commit.clone())) {
                                results.push(m);
                            }
                        }
                    }
                }
                let progress = run.await.map_err(JobError::retry)?;
                if let Some(s) = progress.skipped.iter().find(|s| retryable(s.reason)) {
                    return Err(JobError::retry(format!(
                        "searching {}: {}",
                        range.repo, s.message
                    )));
                }
            }
        }
        results.truncate(MAX_RESULTS);
        Ok(results)
    }
}

/// Skips that fail the job for a retry: the repo node failed or is still cloning.
fn retryable(reason: SkippedReason) -> bool {
    matches!(
        reason,
        SkippedReason::Error | SkippedReason::RepositoryCloning
    )
}

#[async_trait]
impl Handler<TriggerJob> for TriggerHandler {
    async fn handle(&self, _ctx: JobContext, record: Record<TriggerJob>) -> Result<(), JobError> {
        let results = self.search(&record.payload).await?;
        let channels = self.channel_actions(&record.payload, &results).await?;
        self.store
            .finish_trigger_job(record.id, &results, &channels)
            .await
            .map_err(JobError::retry)
    }
}

/// The trigger job worker and its stalled-job resetter, ready to run. `keyring`: see
/// [`TriggerHandler::with_keyring`].
pub fn trigger_workers(
    backend: SearchBackend,
    keyring: Option<Arc<Keyring>>,
    worker_id: &str,
) -> Result<(Worker<TriggerJob>, Resetter<TriggerJob>), sourcegull_pgqueue::Error> {
    let queue: Queue<TriggerJob> = Queue::new(
        backend.db.pool().clone(),
        MonitorStore::trigger_queue_config(),
    )?;
    let mut opts = WorkerOptions::new(worker_id);
    opts.concurrency = 4;
    let worker = Worker::new(
        queue.clone(),
        TriggerHandler::new(backend).with_keyring(keyring),
        opts,
    );
    let resetter = Resetter::new(queue, Duration::from_secs(30));
    Ok((worker, resetter))
}
