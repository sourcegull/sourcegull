//! The evaluator: a leader-elected loop in the `worker` role that turns ref updates
//! into trigger jobs.
//!
//! It keeps a [`MonitorIndex`] of the enabled watches (compiled with their owners'
//! permissions) over the repository catalog, rebuilt when watches change (`NOTIFY
//! code_monitors`), at every sweep (catalog and permission changes) and when an event names a
//! repository the index has not seen.
//!
//! - **Events:** it follows `repo_ref_events` (`NOTIFY repo_ref_events` wakes it, with a poll
//!   fallback). For each repository with new events, it resolves the revisions the watches
//!   watching it care about, and for every head that moved since the last commit searched it
//!   queues the range `last..head`: one trigger job per watch per pass.
//! - **Sweep** (every minute): it checks the pairs never seen (new
//!   watches, repositories that newly match: their heads are recorded without a search),
//!   the repositories whose refs changed since they were last
//!   checked (events that were lost), and watches reset with `resetTriggerQueryTimestamps`
//!   (their whole histories are searched).
//!
//! Watches are not re-run over every repository each minute; the work follows the change
//! rate.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_db::notify::channels;
use sourcegull_db::{Fence, RefEvent};
use sourcegull_rpc::repo::v1 as pb;
use sourcegull_search_web::SearchBackend;
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::index::{CompiledMonitor, Head, MonitorIndex, plan_evaluation};
use crate::store::{CHANNEL, Monitor, MonitorStore};
use crate::trigger::{EvalError, compile_as, owner_actor};

/// Lease name of the evaluator.
pub const EVALUATOR: &str = "code-monitors-evaluator";
/// Counter of trigger jobs queued, by `trigger` (`event`, `sweep`).
pub const TRIGGERS_METRIC: &str = metric_names::CODE_MONITOR_TRIGGERS_TOTAL;

/// Evaluator settings.
#[derive(Debug, Clone)]
pub struct EvaluatorConfig {
    /// Time between sweeps.
    pub sweep_interval: Duration,
    /// Ref events are read at least this often, in case a notification was lost.
    pub poll_interval: Duration,
    /// Ref events per read.
    pub event_batch: i64,
    /// Finished trigger jobs are kept this long.
    pub retention: TimeDelta,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            sweep_interval: Duration::from_secs(60),
            poll_interval: Duration::from_secs(30),
            event_batch: 500,
            retention: TimeDelta::days(30),
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    pub repos: u64,
    pub recorded: u64,
    pub jobs: u64,
}

/// Evaluates watches on ref updates and in sweeps.
pub struct Evaluator {
    backend: SearchBackend,
    store: MonitorStore,
    config: EvaluatorConfig,
    index: Option<Arc<MonitorIndex>>,
    /// Watches whose query no longer compiles for their owner, with the error (logged once).
    broken: HashMap<i64, String>,
    /// The catalog's repositories when the index was built.
    catalog_ids: HashSet<i32>,
    last_sweep: Option<DateTime<Utc>>,
    /// Evaluations and the cursor are written only while this is current.
    fence: Fence,
}

impl Evaluator {
    /// An evaluator that is not fenced (see [`Evaluator::with_fence`]).
    pub fn new(backend: SearchBackend, config: EvaluatorConfig) -> Self {
        let store = MonitorStore::new(&backend.db);
        Self {
            backend,
            store,
            config,
            index: None,
            broken: HashMap::new(),
            catalog_ids: HashSet::new(),
            last_sweep: None,
            fence: Fence::none(EVALUATOR),
        }
    }

    /// Writes evaluations and the cursor only while `fence` is current, so an evaluator that
    /// lost its lease queues no trigger jobs its successor also queues.
    pub fn with_fence(mut self, fence: Fence) -> Self {
        self.fence = fence;
        self
    }

    /// Drops the index; the next pass rebuilds it.
    pub fn invalidate(&mut self) {
        self.index = None;
    }

    /// The current index, rebuilt if needed.
    pub async fn index(&mut self) -> Result<Arc<MonitorIndex>, EvalError> {
        if let Some(i) = &self.index {
            return Ok(i.clone());
        }
        let started = Instant::now();
        let monitors = self.store.list_enabled().await?;
        let mut compiled = Vec::with_capacity(monitors.len());
        let mut broken = HashMap::new();
        for m in &monitors {
            match self.compile(m).await {
                Ok(c) => compiled.push(c),
                Err(EvalError::Db(e)) => return Err(EvalError::Db(e)),
                Err(EvalError::Invalid(e)) => {
                    if self.broken.get(&m.id) != Some(&e) {
                        tracing::warn!(monitor = m.id, error = %e, "code monitor not evaluated");
                    }
                    broken.insert(m.id, e);
                }
            }
        }
        self.broken = broken;
        let catalog = self
            .backend
            .catalog
            .get(&self.backend.db)
            .await
            .map_err(EvalError::Invalid)?;
        self.catalog_ids = catalog.repos().iter().map(|r| r.id).collect();
        let index = Arc::new(MonitorIndex::build(compiled, &catalog));
        tracing::debug!(
            monitors = monitors.len(),
            pairs = index.pairs(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "code monitor index built"
        );
        self.index = Some(index.clone());
        Ok(index)
    }

    async fn compile(&self, m: &Monitor) -> Result<CompiledMonitor, EvalError> {
        let actor = owner_actor(&self.backend.db, m).await?.ok_or_else(|| {
            EvalError::Invalid(
                "the owner no longer exists or is no longer a member of the organization".into(),
            )
        })?;
        compile_as(&self.backend, m.id, &m.query, &actor).await
    }

    /// Resolves the watched revisions of `repos` and queues what moved. `trigger` labels the
    /// metric.
    async fn evaluate(
        &self,
        index: &MonitorIndex,
        repos: &BTreeSet<i32>,
        search_all: &HashSet<i64>,
        trigger: &'static str,
        now: DateTime<Utc>,
    ) -> Result<PassReport, EvalError> {
        let mut report = PassReport::default();
        let mut heads = Vec::new();
        for &repo in repos {
            let targets = index.targets(repo);
            let Some(name) = index.repo_name(repo) else {
                continue;
            };
            if targets.is_empty() {
                continue;
            }
            report.repos += 1;
            let revs: BTreeSet<&str> = targets
                .iter()
                .flat_map(|t| t.revs.iter().map(String::as_str))
                .collect();
            let mut resolved: HashMap<&str, String> = HashMap::new();
            for rev in revs {
                match self.resolve(repo, name, rev).await {
                    Ok(oid) => {
                        resolved.insert(rev, oid);
                    }
                    Err(e) => {
                        tracing::debug!(repo = name, rev, error = %e, "code monitors: revision not resolved");
                    }
                }
            }
            for t in targets {
                for rev in &t.revs {
                    if let Some(oid) = resolved.get(rev.as_str()) {
                        heads.push(Head {
                            repo_id: repo,
                            repo: name.to_owned(),
                            monitor: t.monitor,
                            rev: rev.clone(),
                            oid: oid.clone(),
                        });
                    }
                }
            }
        }
        let monitors: Vec<i64> = heads
            .iter()
            .map(|h| h.monitor)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let last = self.store.last_searched(&monitors).await?;
        let plan = plan_evaluation(&heads, &last, search_all);
        let monitor_queries: HashMap<i64, String> = self
            .store
            .list_enabled()
            .await?
            .into_iter()
            .filter(|m| plan.ranges.contains_key(&m.id))
            .map(|m| (m.id, m.query))
            .collect();
        let jobs: Vec<_> = plan
            .ranges
            .into_iter()
            .filter_map(|(m, ranges)| Some((m, monitor_queries.get(&m)?.clone(), ranges)))
            .collect();
        let searched_all: Vec<i64> = search_all
            .iter()
            .copied()
            .filter(|m| monitors.contains(m))
            .collect();
        let ids = self
            .store
            .record_evaluation(
                &plan.heads,
                &plan.unchanged,
                &jobs,
                &searched_all,
                now,
                &self.fence,
            )
            .await?;
        metrics::counter!(TRIGGERS_METRIC, "trigger" => trigger).increment(ids.len() as u64);
        report.recorded = plan.heads.len() as u64;
        report.jobs = ids.len() as u64;
        Ok(report)
    }

    async fn resolve(&self, repo: i32, name: &str, rev: &str) -> Result<String, String> {
        let mut git = self
            .backend
            .repo_clients
            .for_repo(RepoId(repo))
            .map_err(|e| e.to_string())?;
        let r = git
            .resolve_revision(pb::ResolveRevisionRequest {
                repo: Some(pb::RepoRef {
                    id: repo,
                    name: name.to_owned(),
                }),
                rev: rev.to_owned(),
            })
            .await
            .map_err(|s| s.message().to_owned())?;
        Ok(r.into_inner().commit_oid)
    }

    /// Reads new ref events and evaluates the repositories they name.
    pub async fn handle_events(&mut self, now: DateTime<Utc>) -> Result<PassReport, EvalError> {
        let mut report = PassReport::default();
        loop {
            let after = self.store.cursor().await?;
            let events: Vec<RefEvent> = self
                .backend
                .db
                .ref_events()
                .list_after(after, self.config.event_batch)
                .await?;
            let Some(last) = events.last().map(|e| e.id) else {
                return Ok(report);
            };
            let repos: BTreeSet<i32> = events
                .iter()
                .filter(|e| e.source == "fetch")
                .map(|e| e.repo_id.get())
                .collect();
            if !repos.is_empty() {
                let mut index = self.index().await?;
                // A repository the catalog did not have when the index was built may be new.
                let unknown: Vec<i32> = repos
                    .iter()
                    .copied()
                    .filter(|r| !self.catalog_ids.contains(r))
                    .collect();
                if !unknown.is_empty() {
                    let catalog = self
                        .backend
                        .catalog
                        .get(&self.backend.db)
                        .await
                        .map_err(EvalError::Invalid)?;
                    if unknown.iter().any(|r| catalog.get(*r).is_some()) {
                        self.invalidate();
                        index = self.index().await?;
                    }
                }
                let r = self
                    .evaluate(&index, &repos, &HashSet::new(), "event", now)
                    .await?;
                report.repos += r.repos;
                report.recorded += r.recorded;
                report.jobs += r.jobs;
            }
            self.store.set_cursor(last, &self.fence).await?;
        }
    }

    /// One sweep (see the module docs).
    pub async fn sweep(&mut self, now: DateTime<Utc>) -> Result<PassReport, EvalError> {
        self.invalidate();
        let index = self.index().await?;
        let monitors = self.store.list_enabled().await?;
        let search_all: HashSet<i64> = monitors
            .iter()
            .filter(|m| m.search_all)
            .map(|m| m.id)
            .collect();
        let query_changed: HashMap<i64, DateTime<Utc>> = monitors
            .iter()
            .map(|m| (m.id, m.query_updated_at.max(m.updated_at)))
            .collect();
        let last = self.store.last_searched(&index.monitor_ids()).await?;
        // (watch, repo) → oldest check.
        let mut checked: HashMap<(i64, i32), DateTime<Utc>> = HashMap::new();
        for l in &last {
            let e = checked
                .entry((l.monitor_id, l.repo_id.get()))
                .or_insert(l.checked_at);
            *e = (*e).min(l.checked_at);
        }
        let repos: Vec<i32> = index.repos().collect();
        let changed: HashMap<i32, DateTime<Utc>> = self
            .store
            .repos_last_changed(&repos)
            .await?
            .into_iter()
            .map(|(r, at)| (r.get(), at))
            .collect();
        let mut due = BTreeSet::new();
        for &repo in &repos {
            let is_due = index.targets(repo).iter().any(|t| {
                if search_all.contains(&t.monitor) {
                    return true;
                }
                match checked.get(&(t.monitor, repo)) {
                    None => true,
                    Some(at) => {
                        changed.get(&repo).is_some_and(|c| c > at)
                            || query_changed.get(&t.monitor).is_some_and(|q| q > at)
                    }
                }
            });
            if is_due {
                due.insert(repo);
            }
        }
        let report = self
            .evaluate(&index, &due, &search_all, "sweep", now)
            .await?;
        // Reset watches that match no repository at all are done too.
        let idle: Vec<i64> = search_all
            .iter()
            .copied()
            .filter(|m| {
                !index
                    .repos()
                    .any(|r| index.targets(r).iter().any(|t| t.monitor == *m))
            })
            .collect();
        if !idle.is_empty() {
            self.store
                .record_evaluation(&[], &[], &[], &idle, now, &self.fence)
                .await?;
        }
        let collect_before = self
            .last_sweep
            .is_none_or(|at| now - at > TimeDelta::hours(1));
        if collect_before {
            let n = self
                .store
                .delete_finished_before(now - self.config.retention)
                .await?;
            if n > 0 {
                tracing::info!(deleted = n, "code monitors: old trigger jobs deleted");
            }
            self.last_sweep = Some(now);
        }
        Ok(report)
    }
}

fn log_pass(what: &str, r: Result<PassReport, EvalError>) {
    match r {
        Ok(r) if r.jobs > 0 || r.recorded > 0 => tracing::info!(
            pass = what,
            repos = r.repos,
            recorded = r.recorded,
            jobs = r.jobs,
            "code monitors evaluated"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(pass = what, error = %e, "code monitor evaluation"),
    }
}

/// What woke the loop.
enum Wake {
    Events,
    Monitors,
}

/// Runs the evaluator, fenced by `fence`, until `cancel` fires.
pub async fn run_evaluator(
    backend: SearchBackend,
    config: EvaluatorConfig,
    fence: Fence,
    cancel: CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Wake>(64);
    let listener = tokio::spawn(listen(backend.clone(), tx, cancel.clone()));
    let mut sweep = tokio::time::interval(config.sweep_interval);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut poll = tokio::time::interval(config.poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ev = Evaluator::new(backend, config).with_fence(fence);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            w = rx.recv() => match w {
                Some(Wake::Events) => log_pass("events", sourcegull_db::record_run(EVALUATOR, ev.handle_events(Utc::now())).await),
                // A new or changed watch: rebuild and look at its repositories now.
                Some(Wake::Monitors) => log_pass("sweep", sourcegull_db::record_run(EVALUATOR, ev.sweep(Utc::now())).await),
                None => break,
            },
            _ = poll.tick() => log_pass("events", sourcegull_db::record_run(EVALUATOR, ev.handle_events(Utc::now())).await),
            _ = sweep.tick() => log_pass("sweep", sourcegull_db::record_run(EVALUATOR, ev.sweep(Utc::now())).await),
        }
    }
    listener.abort();
}

async fn listen(
    backend: SearchBackend,
    tx: tokio::sync::mpsc::Sender<Wake>,
    cancel: CancellationToken,
) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(backend.db.pool(), &[channels::REPO_REF_EVENTS, CHANNEL]) => s,
    };
    loop {
        let wake = tokio::select! {
            () = cancel.cancelled() => return,
            e = sub.recv() => match e {
                sourcegull_pubsub::Event::Notification { channel, .. } if channel == CHANNEL => Wake::Monitors,
                // Ref events, or a resync: events may have been missed.
                _ => Wake::Events,
            },
        };
        // A full queue already holds a wake-up.
        let _ = tx.try_send(wake);
    }
}
