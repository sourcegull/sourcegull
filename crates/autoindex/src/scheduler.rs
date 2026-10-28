//! The auto-indexing scheduler: a leader-elected loop in the `worker` role.
//!
//! Event-driven: it follows `repo_ref_events` (rows a repository node writes after a fetch moved
//! refs; `NOTIFY repo_ref_events` wakes it, with a poll fallback). For each moved branch or tag
//! that a configuration policy with indexing enabled names, it queues the new commit, if the commit
//! is younger than the policy's `indexCommitMaxAgeHours`; for branch policies with
//! `indexIntermediateCommits`, the commits the push added too (up to
//! [`SchedulerConfig::max_intermediate_commits`]). Each repository queues at most
//! `codeIntelAutoIndexing.repositoryCommitsPerHour` commits this way.
//!
//! A periodic sweep catches what events missed: it takes the repositories indexing policies apply
//! to that it has not scanned within [`SchedulerConfig::repo_process_delay`] and queues the commits
//! the policies name. Queueing deduplicates per commit and per (root, indexer)
//! ([`crate::service`]).
//!
//! Nothing is scheduled unless `codeIntelAutoIndexing.enabled` is set; global policies only
//! count with `codeIntelAutoIndexing.allowGlobalPolicies`.

use sourcegull_codeintel_policies::store::{ConfigurationPolicy, ListPolicies, PolicyStore};
use sourcegull_codeintel_scip::store::IndexJobStore;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::store::AutoIndexStore;
use chrono::{DateTime, TimeDelta, Utc};
use sourcegull_codeintel_policies::{Purpose, RepoGit, match_policies};
use sourcegull_config::SiteConfig;
use sourcegull_db::RefEvent;
use sourcegull_db::notify::channels;
use sourcegull_services::git::GitReader;
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;
use tokio_util::sync::CancellationToken;

use crate::service::{AutoIndexError, AutoIndexer, RepoRef};

/// Lease name of the scheduler.
pub const AUTOINDEX_SCHEDULER: &str = "codeintel-autoindex-scheduler";
/// Counter of commits considered for queueing, by trigger (`event`, `sweep`).
pub const COMMITS_METRIC: &str = metric_names::AUTOINDEX_COMMITS_CONSIDERED_TOTAL;
/// Counter of commits not queued because of the per-repository rate limit.
pub const RATE_LIMITED_METRIC: &str = metric_names::AUTOINDEX_RATE_LIMITED_TOTAL;

/// Scheduler settings.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Time between sweeps (default: 2 minutes).
    pub sweep_interval: Duration,
    /// A repository is swept at most this often (default: 24 hours).
    pub repo_process_delay: Duration,
    /// Repositories per sweep.
    pub repo_batch: i64,
    /// Ref events are read at least this often, in case a notification was lost.
    pub poll_interval: Duration,
    /// Ref events per read.
    pub event_batch: i64,
    /// Intermediate commits queued per pushed branch or swept branch.
    pub max_intermediate_commits: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            sweep_interval: Duration::from_secs(120),
            repo_process_delay: Duration::from_secs(24 * 3600),
            repo_batch: 100,
            poll_interval: Duration::from_secs(30),
            event_batch: 500,
            max_intermediate_commits: 50,
        }
    }
}

/// What one pass queued.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    pub repos: u64,
    pub commits: u64,
    pub jobs: u64,
    pub rate_limited: u64,
}

impl PassReport {
    fn add(&mut self, other: &PassReport) {
        self.repos += other.repos;
        self.commits += other.commits;
        self.jobs += other.jobs;
        self.rate_limited += other.rate_limited;
    }
}

/// Runs the scheduler until `cancel` fires.
pub async fn run_scheduler(
    indexer: AutoIndexer,
    config: SchedulerConfig,
    cancel: CancellationToken,
) {
    let wake = Arc::new(tokio::sync::Notify::new());
    let listener = tokio::spawn(listen(indexer.clone(), wake.clone(), cancel.clone()));
    let mut sweep = tokio::time::interval(config.sweep_interval);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut poll = tokio::time::interval(config.poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            () = wake.notified() => log_pass("events", sourcegull_db::record_run(AUTOINDEX_SCHEDULER, handle_events(&indexer, &config, Utc::now())).await),
            _ = poll.tick() => log_pass("events", sourcegull_db::record_run(AUTOINDEX_SCHEDULER, handle_events(&indexer, &config, Utc::now())).await),
            _ = sweep.tick() => log_pass("sweep", sourcegull_db::record_run(AUTOINDEX_SCHEDULER, sweep_pass(&indexer, &config, Utc::now())).await),
        }
    }
    listener.abort();
}

fn log_pass(what: &str, r: Result<PassReport, AutoIndexError>) {
    match r {
        Ok(r) if r.jobs > 0 || r.rate_limited > 0 => tracing::info!(
            pass = what,
            repos = r.repos,
            commits = r.commits,
            jobs = r.jobs,
            rate_limited = r.rate_limited,
            "auto-indexing scheduler"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(pass = what, error = %e, "auto-indexing scheduler"),
    }
}

async fn listen(indexer: AutoIndexer, wake: Arc<tokio::sync::Notify>, cancel: CancellationToken) {
    let mut sub = tokio::select! {
        () = cancel.cancelled() => return,
        s = sourcegull_pubsub::subscribe(indexer.db().pool(), &[channels::REPO_REF_EVENTS]) => s,
    };
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            // A notification, or a resync after missed ones.
            _ = sub.recv() => wake.notify_one(),
        }
    }
}

/// The policies with indexing enabled that may schedule, given the site config.
async fn indexing_policies(
    indexer: &AutoIndexer,
    site: &SiteConfig,
    repo: Option<&RepoRef>,
) -> Result<Vec<ConfigurationPolicy>, AutoIndexError> {
    let policies = PolicyStore::new(indexer.db())
        .list(&ListPolicies {
            repo: repo.map(|r| (r.id, r.name.clone())),
            for_indexing: Some(true),
            ..Default::default()
        })
        .await?;
    Ok(policies
        .into_iter()
        .filter(|p| site.code_intel_auto_indexing_allow_global_policies || !p.is_global())
        .collect())
}

/// Reads new ref events and queues the commits they bring that policies name.
pub async fn handle_events(
    indexer: &AutoIndexer,
    config: &SchedulerConfig,
    now: DateTime<Utc>,
) -> Result<PassReport, AutoIndexError> {
    let store = AutoIndexStore::new(indexer.db());
    let mut report = PassReport::default();
    loop {
        let after = store.cursor().await?;
        let events = indexer
            .db()
            .ref_events()
            .list_after(after, config.event_batch)
            .await?;
        let Some(last) = events.last().map(|e| e.id) else {
            return Ok(report);
        };
        if indexer.enabled() {
            let mut by_repo: BTreeMap<RepoId, Vec<RefEvent>> = BTreeMap::new();
            for e in events {
                let interesting = e.source == "fetch"
                    && e.new_oid.is_some()
                    && (e.ref_name.starts_with("refs/heads/")
                        || e.ref_name.starts_with("refs/tags/"));
                if interesting {
                    by_repo.entry(e.repo_id).or_default().push(e);
                }
            }
            for (repo, events) in by_repo {
                match handle_repo_events(indexer, config, repo, &events, now).await {
                    Ok(r) => report.add(&r),
                    Err(e) => {
                        tracing::warn!(repo = repo.get(), error = %e, "auto-indexing ref events");
                    }
                }
            }
        }
        store.set_cursor(last).await?;
    }
}

async fn repo_ref(indexer: &AutoIndexer, id: RepoId) -> Result<Option<RepoRef>, AutoIndexError> {
    Ok(indexer
        .db()
        .repos()
        // An internal job: it reads every repository.
        .get_authorized(id, &sourcegull_types::RepoAccess::All)
        .await?
        .map(|r| RepoRef { id, name: r.name }))
}

async fn handle_repo_events(
    indexer: &AutoIndexer,
    config: &SchedulerConfig,
    id: RepoId,
    events: &[RefEvent],
    now: DateTime<Utc>,
) -> Result<PassReport, AutoIndexError> {
    let mut report = PassReport::default();
    let Some(repo) = repo_ref(indexer, id).await? else {
        return Ok(report);
    };
    let site = indexer.site_config();
    let policies = indexing_policies(indexer, &site, Some(&repo)).await?;
    if policies.is_empty() {
        return Ok(report);
    }
    report.repos = 1;
    let git = RepoGit::new(indexer.clients().clone(), repo.id, &repo.name);
    let matches = match_policies(&git, &policies, Purpose::Indexing, now).await?;
    let reader = GitReader::from_parts(indexer.clients().clone(), repo.id, &repo.name);
    let mut commits: BTreeSet<String> = BTreeSet::new();
    for e in events {
        let Some(new) = e.new_oid.as_deref() else {
            continue;
        };
        let short = e
            .ref_name
            .strip_prefix("refs/heads/")
            .or_else(|| e.ref_name.strip_prefix("refs/tags/"))
            .unwrap_or(&e.ref_name);
        // Policies name the ref (and the commit is young enough).
        if matches
            .tips
            .iter()
            .any(|t| t.policy.is_some() && t.name == short && t.commit == new)
        {
            commits.insert(new.to_owned());
        }
        // Intermediate commits the push added to a branch.
        if let Some(b) = matches
            .branches
            .iter()
            .find(|b| b.name == short && b.tip == new)
        {
            let mut hide: Vec<String> = e.old_oid.iter().cloned().collect();
            if !b.is_default {
                hide.extend(matches.head.iter().cloned());
            }
            let max_age = b
                .policies
                .iter()
                .map(|(_, d)| *d)
                .max_by(cmp_duration)
                .flatten();
            for c in added_commits(
                &reader,
                new,
                &hide,
                config.max_intermediate_commits,
                max_age,
                now,
            )
            .await?
            {
                commits.insert(c);
            }
        }
    }
    let limit = i32::try_from(site.code_intel_auto_indexing_repository_commits_per_hour)
        .unwrap_or(i32::MAX)
        .max(1);
    for commit in commits {
        metrics::counter!(COMMITS_METRIC, "trigger" => "event").increment(1);
        report.commits += 1;
        if IndexJobStore::new(indexer.db())
            .is_queued(repo.id, &commit)
            .await?
        {
            continue;
        }
        if !AutoIndexStore::new(indexer.db())
            .take_rate_limit(repo.id, limit, TimeDelta::hours(1), now)
            .await?
        {
            metrics::counter!(RATE_LIMITED_METRIC).increment(1);
            report.rate_limited += 1;
            continue;
        }
        report.jobs += indexer
            .queue(&repo, &commit, None, false, None)
            .await?
            .len() as u64;
    }
    Ok(report)
}

/// `None` (no limit) is the longest duration.
fn cmp_duration(a: &Option<TimeDelta>, b: &Option<TimeDelta>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

/// The commits reachable from `tip` but not from `hide`, newest first, at most `limit`, no
/// older than `max_age`.
async fn added_commits(
    reader: &GitReader,
    tip: &str,
    hide: &[String],
    limit: u32,
    max_age: Option<TimeDelta>,
    now: DateTime<Utc>,
) -> Result<Vec<String>, AutoIndexError> {
    let (commits, _) = reader.commit_log_hiding(tip, hide, "", 0, limit).await?;
    Ok(commits
        .into_iter()
        .filter(|c| {
            let at = c
                .committer
                .as_ref()
                .and_then(|s| DateTime::from_timestamp(s.time, 0))
                .unwrap_or(now);
            max_age.is_none_or(|d| now - at <= d)
        })
        .map(|c| c.oid)
        .collect())
}

/// One sweep at `now` (see the module docs).
pub async fn sweep_pass(
    indexer: &AutoIndexer,
    config: &SchedulerConfig,
    now: DateTime<Utc>,
) -> Result<PassReport, AutoIndexError> {
    let mut report = PassReport::default();
    if !indexer.enabled() {
        return Ok(report);
    }
    let site = indexer.site_config();
    let policies = indexing_policies(indexer, &site, None).await?;
    if policies.is_empty() {
        return Ok(report);
    }
    let repos = AutoIndexStore::new(indexer.db()).cloned_repos().await?;
    let limit = usize::try_from(site.code_intel_auto_indexing_policy_repository_match_limit).ok();
    let mut candidates: BTreeSet<RepoId> = BTreeSet::new();
    for p in &policies {
        let matching = repos.iter().filter(|(id, name)| p.applies_to(*id, name));
        match (limit, p.repository_patterns.is_some()) {
            (Some(n), true) => candidates.extend(matching.take(n).map(|(id, _)| *id)),
            _ => candidates.extend(matching.map(|(id, _)| *id)),
        }
    }
    let candidates: Vec<RepoId> = candidates.into_iter().collect();
    let before = now - TimeDelta::from_std(config.repo_process_delay).unwrap_or(TimeDelta::MAX);
    let store = AutoIndexStore::new(indexer.db());
    for id in store
        .repos_to_scan(&candidates, before, config.repo_batch)
        .await?
    {
        store.mark_scanned(id, now).await?;
        let Some(name) = repos.iter().find(|(r, _)| *r == id).map(|(_, n)| n.clone()) else {
            continue;
        };
        let repo = RepoRef { id, name };
        match sweep_repo(indexer, config, &site, &repo, now).await {
            Ok(r) => report.add(&r),
            Err(e) => tracing::warn!(repo = %repo.name, error = %e, "auto-indexing sweep"),
        }
    }
    Ok(report)
}

async fn sweep_repo(
    indexer: &AutoIndexer,
    config: &SchedulerConfig,
    site: &SiteConfig,
    repo: &RepoRef,
    now: DateTime<Utc>,
) -> Result<PassReport, AutoIndexError> {
    let mut report = PassReport {
        repos: 1,
        ..Default::default()
    };
    let policies = indexing_policies(indexer, site, Some(repo)).await?;
    if policies.is_empty() {
        return Ok(report);
    }
    let git = RepoGit::new(indexer.clients().clone(), repo.id, &repo.name);
    let matches = match_policies(&git, &policies, Purpose::Indexing, now).await?;
    let reader = GitReader::from_parts(indexer.clients().clone(), repo.id, &repo.name);
    let mut commits: BTreeSet<String> = matches
        .tips
        .iter()
        .filter(|t| t.policy.is_some())
        .map(|t| t.commit.clone())
        .collect();
    for b in &matches.branches {
        let hide: Vec<String> = if b.is_default {
            Vec::new()
        } else {
            matches.head.iter().cloned().collect()
        };
        let max_age = b
            .policies
            .iter()
            .map(|(_, d)| *d)
            .max_by(cmp_duration)
            .flatten();
        commits.extend(
            added_commits(
                &reader,
                &b.tip,
                &hide,
                config.max_intermediate_commits,
                max_age,
                now,
            )
            .await?,
        );
    }
    for commit in commits {
        metrics::counter!(COMMITS_METRIC, "trigger" => "sweep").increment(1);
        report.commits += 1;
        match indexer.queue(repo, &commit, None, false, None).await {
            Ok(ids) => report.jobs += ids.len() as u64,
            Err(AutoIndexError::Git(e)) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(report)
}
