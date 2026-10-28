//! Periodic fetch scheduling and repo node assignment.
//!
//! Each node schedules only the repos assigned to it, so no lease is needed: every pass it
//! enqueues a LOW-priority (`scheduled`) update for each of its repos whose
//! [`next_fetch`](sourcegull_gitstore::schedule::next_fetch) time has come and that has no update
//! queued or running. Webhooks, discovery and "update now" enqueue at higher priorities
//! elsewhere; the queue coalesces them per repo.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_config::SiteConfig;
use sourcegull_db::{DbError, FetchSchedule, RepoUpdatePriority, RepoUpdateReason};
use sourcegull_gitstore::schedule::{
    FetchHistory, NextFetch, SchedulePolicy, UpdateIntervalRules, next_fetch,
};
use sourcegull_types::RepoId;
use sourcegull_types::metric_names;

use super::RepoNode;

/// Repos assigned per batch by [`RepoNode::assign_unassigned`].
const ASSIGN_BATCH: i64 = 1_000;
/// Batches per pass, so one pass stays short on a large first discovery.
const ASSIGN_MAX_BATCHES: usize = 20;
/// Due repos looked at per scheduler pass.
const SCHEDULE_BATCH: i64 = 2_000;

/// The fetch policy inputs taken from the site config.
pub(crate) struct Policy {
    policy: SchedulePolicy,
    rules: UpdateIntervalRules,
    auto_updates: bool,
}

impl Policy {
    /// Builds the policy from `disableAutoGitUpdates` and `gitUpdateInterval`. Rules with an
    /// invalid pattern are skipped with a warning.
    pub(crate) fn from_config(config: &SiteConfig) -> Self {
        let mut valid = Vec::new();
        for rule in &config.git_update_interval {
            match UpdateIntervalRules::new([(
                rule.pattern.as_str(),
                Duration::from_secs(u64::from(rule.interval.max(1)) * 60),
            )]) {
                Ok(_) => valid.push((
                    rule.pattern.as_str(),
                    Duration::from_secs(u64::from(rule.interval.max(1)) * 60),
                )),
                Err(e) => tracing::warn!(error = %e, "ignoring gitUpdateInterval rule"),
            }
        }
        Self {
            policy: SchedulePolicy::default(),
            rules: UpdateIntervalRules::new(valid).unwrap_or_default(),
            auto_updates: !config.disable_auto_git_updates,
        }
    }

    /// When `repo` should be fetched next; `None` when automatic updates are off.
    pub(crate) fn next_due(
        &self,
        repo: RepoId,
        name: &str,
        s: &FetchSchedule,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        match next_fetch(
            repo,
            &history(s),
            &self.policy,
            self.rules.interval_for(name),
            self.auto_updates,
        ) {
            NextFetch::Now => Some(now),
            NextFetch::At(t) => Some(t),
            NextFetch::Never => None,
        }
    }
}

/// The policy's view of a `gitserver_repos` row.
pub(crate) fn history(s: &FetchSchedule) -> FetchHistory {
    FetchHistory {
        last_fetched: s.last_fetched,
        last_changed: s.last_changed,
        mean_change_interval: s
            .mean_change_interval_secs
            .filter(|m| m.is_finite() && *m >= 0.0)
            .map(Duration::from_secs_f64),
        consecutive_failures: u32::try_from(s.consecutive_failures).unwrap_or(0),
        last_attempt: s.last_attempt_at,
    }
}

#[derive(sqlx::FromRow)]
struct DueRow {
    repo_id: RepoId,
    name: String,
    #[sqlx(flatten)]
    schedule: FetchSchedule,
}

impl RepoNode {
    /// Persists the hash assignment of repos that have none in `repo.shard`. Every node computes
    /// the same assignment from the same node list, so running this on all nodes at once is safe.
    /// Returns the number of repos assigned.
    pub async fn assign_unassigned(&self) -> Result<u64, DbError> {
        let repos = self.inner.db.repos();
        let mut total = 0;
        for _ in 0..ASSIGN_MAX_BATCHES {
            let ids = repos.unassigned(ASSIGN_BATCH).await?;
            if ids.is_empty() {
                break;
            }
            let full = ids.len() as i64 == ASSIGN_BATCH;
            let assignments: Vec<(RepoId, String)> = ids
                .into_iter()
                .filter_map(|id| {
                    self.inner
                        .router
                        .hashed_node_for(id)
                        .map(|n| (id, n.to_owned()))
                })
                .collect();
            let n = repos.assign_shards(&assignments).await?;
            total += n;
            if !full || n == 0 {
                break;
            }
        }
        if total > 0 {
            tracing::info!(assigned = total, "assigned repos to repo nodes");
        }
        Ok(total)
    }

    /// One scheduler pass: enqueues a `scheduled` update for every repo of this node that is
    /// due and has nothing queued or running, and stores the due time of the others. Returns
    /// the number of updates enqueued.
    pub async fn schedule_once(&self) -> Result<usize, DbError> {
        let inner = &*self.inner;
        let policy = Policy::from_config(&inner.site.load().value);
        if !policy.auto_updates {
            return Ok(0);
        }
        let rows: Vec<DueRow> = sqlx::query_as(
            "SELECT g.repo_id, r.name, g.last_fetched, g.last_changed, g.last_attempt_at,
                    g.consecutive_failures, g.mean_change_interval_secs, g.next_fetch_at
             FROM gitserver_repos g JOIN repo r ON r.id = g.repo_id
             WHERE r.shard = $1 AND r.deleted_at IS NULL AND r.blocked IS NULL
               AND (g.next_fetch_at IS NULL OR g.next_fetch_at <= now())
               AND NOT EXISTS (SELECT 1 FROM repo_update_jobs j
                               WHERE j.repo_id = g.repo_id AND j.state IN ('queued', 'processing'))
             ORDER BY g.next_fetch_at NULLS FIRST, g.repo_id
             LIMIT $2",
        )
        .bind(&inner.me)
        .bind(SCHEDULE_BATCH)
        .fetch_all(inner.db.pool())
        .await?;
        let now = Utc::now();
        let mut enqueued = 0;
        for row in rows {
            match policy.next_due(row.repo_id, &row.name, &row.schedule, now) {
                Some(at) if at <= now => {
                    inner
                        .db
                        .repo_update_jobs()
                        .enqueue(
                            row.repo_id,
                            RepoUpdatePriority::LOW,
                            RepoUpdateReason::Scheduled,
                        )
                        .await?;
                    enqueued += 1;
                }
                at => {
                    inner
                        .db
                        .gitserver_repos()
                        .set_next_fetch_at(row.repo_id, at)
                        .await?;
                }
            }
        }
        if enqueued > 0 {
            tracing::debug!(enqueued, "queued scheduled repo fetches");
        }
        metrics::counter!(metric_names::REPO_SCHEDULED_FETCHES_TOTAL).increment(enqueued as u64);
        Ok(enqueued)
    }

    /// Runs [`assign_unassigned`](Self::assign_unassigned) and
    /// [`schedule_once`](Self::schedule_once) every scheduler interval until `cancel` fires.
    pub(crate) async fn run_scheduler(self, cancel: tokio_util::sync::CancellationToken) {
        let mut tick = tokio::time::interval(self.inner.cfg.scheduler_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = tick.tick() => {}
            }
            if let Err(e) = self.assign_unassigned().await {
                tracing::warn!(error = %e, "assigning repos to repo nodes failed");
            }
            if let Err(e) = self.schedule_once().await {
                tracing::warn!(error = %e, "scheduling repo fetches failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sourcegull_config::GitUpdateIntervalRule;

    #[test]
    fn policy_follows_site_config() {
        let now = Utc::now();
        let mut config = SiteConfig::default();
        let never = FetchSchedule::default();
        let p = Policy::from_config(&config);
        assert_eq!(p.next_due(RepoId(1), "a", &never, now), Some(now));

        let fetched = FetchSchedule {
            last_fetched: Some(now),
            last_attempt_at: Some(now),
            ..Default::default()
        };
        config.git_update_interval = vec![
            GitUpdateIntervalRule {
                pattern: "(".into(),
                interval: 1,
            },
            GitUpdateIntervalRule {
                pattern: "^big/".into(),
                interval: 10,
            },
        ];
        let p = Policy::from_config(&config);
        let due = p.next_due(RepoId(1), "big/x", &fetched, now).unwrap();
        let secs = (due - now).num_seconds();
        assert!((540..=660).contains(&secs), "{secs}");

        config.disable_auto_git_updates = true;
        let p = Policy::from_config(&config);
        assert_eq!(p.next_due(RepoId(1), "a", &never, now), None);
    }
}
