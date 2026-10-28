//! The upload expirer: a leader-elected loop in the `worker` role that marks
//! uploads no retention policy keeps any more as `expired`. The upload janitor then deletes
//! expired uploads no other upload depends on.
//!
//! Each pass takes the repositories scanned longest ago (at most once per
//! [`ExpirerConfig::repo_process_delay`]), matches their retention policies, and checks their
//! unexpired completed uploads not checked within [`ExpirerConfig::upload_process_delay`].
//! An upload whose commit no longer exists (a force-push removed it) is deleted outright.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_codeintel_scip::store::CodeIntelStore;
use sourcegull_db::Db;
use sourcegull_rpc::RepoClients;
use sourcegull_types::metric_names;
use sourcegull_types::{RepoAccess, RepoId};
use tokio_util::sync::CancellationToken;

use crate::PolicyError;
use crate::git::RepoGit;
use crate::retention::Retention;

/// Lease name of the expirer.
pub const UPLOAD_EXPIRER: &str = "codeintel-upload-expirer";
/// Counter of uploads checked for retention.
pub const SCANNED_METRIC: &str = metric_names::CODEINTEL_RETENTION_UPLOADS_SCANNED_TOTAL;
/// Counter of uploads marked expired.
pub const EXPIRED_METRIC: &str = metric_names::CODEINTEL_RETENTION_UPLOADS_EXPIRED_TOTAL;
/// Counter of uploads deleted because their commit is gone.
pub const UNKNOWN_COMMIT_METRIC: &str = metric_names::CODEINTEL_RETENTION_UNKNOWN_COMMITS_TOTAL;

/// Expirer settings.
#[derive(Debug, Clone)]
pub struct ExpirerConfig {
    /// Time between passes.
    pub interval: Duration,
    /// Repositories scanned per pass.
    pub repo_batch: i64,
    /// A repository is scanned at most this often.
    pub repo_process_delay: Duration,
    /// Uploads checked per query.
    pub upload_batch: i64,
    /// An upload is checked at most this often.
    pub upload_process_delay: Duration,
}

impl Default for ExpirerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            repo_batch: 100,
            repo_process_delay: Duration::from_secs(24 * 3600),
            upload_batch: 100,
            upload_process_delay: Duration::from_secs(24 * 3600),
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpirerReport {
    pub repos: u64,
    pub scanned: u64,
    pub expired: u64,
    pub unknown_commits: u64,
}

/// Runs passes every [`ExpirerConfig::interval`] (the first right away) until `cancel` fires.
pub async fn run_expirer(
    db: Db,
    clients: Arc<RepoClients>,
    config: ExpirerConfig,
    cancel: CancellationToken,
) {
    let mut tick = tokio::time::interval(config.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let Ok(r) = sourcegull_db::record_run(UPLOAD_EXPIRER, async {
            Ok::<_, std::convert::Infallible>(expire_pass(&db, &clients, &config, Utc::now()).await)
        })
        .await;
        if r.expired > 0 || r.unknown_commits > 0 {
            tracing::info!(
                repos = r.repos,
                scanned = r.scanned,
                expired = r.expired,
                unknown_commits = r.unknown_commits,
                "upload expirer pass"
            );
        }
    }
}

fn ago(now: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    now - chrono::TimeDelta::from_std(d).unwrap_or(chrono::TimeDelta::MAX)
}

/// One pass at `now`. Failures are logged per repository; a repository that fails is
/// retried after its process delay.
pub async fn expire_pass(
    db: &Db,
    clients: &Arc<RepoClients>,
    config: &ExpirerConfig,
    now: DateTime<Utc>,
) -> ExpirerReport {
    let mut report = ExpirerReport::default();
    let repos = match CodeIntelStore::new(db)
        .repos_for_retention_scan(ago(now, config.repo_process_delay), now, config.repo_batch)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "selecting repositories for the upload expirer");
            return report;
        }
    };
    for repo in repos {
        report.repos += 1;
        if let Err(e) = expire_repo(db, clients, config, repo, now, &mut report).await {
            tracing::warn!(repo = repo.get(), error = %e, "upload expirer");
        }
    }
    metrics::counter!(SCANNED_METRIC).increment(report.scanned);
    metrics::counter!(EXPIRED_METRIC).increment(report.expired);
    metrics::counter!(UNKNOWN_COMMIT_METRIC).increment(report.unknown_commits);
    report
}

async fn expire_repo(
    db: &Db,
    clients: &Arc<RepoClients>,
    config: &ExpirerConfig,
    repo: RepoId,
    now: DateTime<Utc>,
    report: &mut ExpirerReport,
) -> Result<(), PolicyError> {
    // An internal job: it reads every repository.
    let Some(r) = db.repos().get_authorized(repo, &RepoAccess::All).await? else {
        return Ok(());
    };
    let git = RepoGit::new(clients.clone(), repo, &r.name);
    let retention = Retention::load(&git, db, now).await?;
    let store = CodeIntelStore::new(db);
    // Checked uploads get `now`, so each query returns new ones until none are left.
    let before = ago(now, config.upload_process_delay);
    loop {
        let uploads = store
            .uploads_for_retention(repo, before, config.upload_batch)
            .await?;
        if uploads.is_empty() {
            return Ok(());
        }
        let (mut protected, mut expired) = (Vec::new(), Vec::new());
        for u in &uploads {
            report.scanned += 1;
            if !git.has_commit(&u.commit).await? {
                store.mark_deleting(&[u.id], "unknown commit").await?;
                report.unknown_commits += 1;
                continue;
            }
            if retention.protections(u, now, true).await?.is_empty() {
                expired.push(u.id);
            } else {
                protected.push(u.id);
            }
        }
        store.update_retention(&protected, &expired, now).await?;
        report.expired += expired.len() as u64;
    }
}
