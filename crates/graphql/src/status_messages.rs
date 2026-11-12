//! `statusMessages`: what the site admin's status indicator in the top navigation
//! shows, computed on each call: auto updates disabled,
//! no repositories, code host connections whose last sync failed, repos whose last clone or
//! fetch failed, clone and index progress, repo nodes that do not answer (their repositories
//! are left out of search), repo nodes low on disk space, and configuration
//! files (SOURCEGULL_RESOURCES_FILE, SOURCEGULL_GLOBAL_SETTINGS_FILE) that could not be applied.
//!
//! Sourcegull reports a repo node that paused clones for lack of disk space (the repos it skipped
//! carry [`sourcegull_db::CLONES_PAUSED_ERROR`]) as `GitserverDiskThresholdReached`, next to the
//! disk usage threshold (90 %, asked from the configured nodes with a short
//! timeout). `CloningProgress` also carries the counts behind its message.
//!
//! Site admins only.

use std::time::Duration;

use async_graphql::{Context, Enum, Object, Result, SimpleObject, Union};
use sourcegull_db::RepoNodeAvailability;
use sourcegull_rpc::repo::v1 as pb;

use crate::context::{db_error, require_site_admin, services, to_int};
use crate::external_services::ExternalServiceNode;

/// Disk usage (percent) from which a repo node gets a message (the default of
/// `gitserverDiskUsageWarningThreshold`).
const DISK_WARNING_PERCENT: f64 = 90.0;

/// How long `statusMessages` waits for a repo node's disk usage; the indicator polls, so a
/// node that does not answer in time is reported as unavailable rather than holding the request.
const DISK_INFO_TIMEOUT: Duration = Duration::from_secs(2);

/// `GitUpdatesDisabled`: `disableAutoGitUpdates` is set.
#[derive(SimpleObject)]
pub(crate) struct GitUpdatesDisabled {
    /// The message.
    message: String,
}

/// `NoRepositoriesDetected`: there are no repositories.
#[derive(SimpleObject)]
pub(crate) struct NoRepositoriesDetected {
    /// The message.
    message: String,
}

/// `CloningProgress`: repositories are queued for their first clone or being cloned.
#[derive(SimpleObject)]
pub(crate) struct CloningProgress {
    /// The message.
    message: String,
    /// Cloned repositories.
    cloned: i32,
    /// Repositories being cloned.
    cloning: i32,
    /// Repositories waiting for their first clone, without a failed attempt.
    queued: i32,
    /// All repositories.
    total: i32,
}

/// `ExternalServiceSyncError`: the last sync of a code host connection failed.
pub(crate) struct ExternalServiceSyncError {
    message: String,
    service: sourcegull_types::ExternalService,
}

#[Object]
impl ExternalServiceSyncError {
    /// The message.
    async fn message(&self) -> &str {
        &self.message
    }

    /// The connection that failed to sync.
    async fn external_service(&self) -> ExternalServiceNode {
        ExternalServiceNode::new(self.service.clone())
    }
}

/// `SyncError`: repositories whose last clone or fetch failed.
#[derive(SimpleObject)]
pub(crate) struct SyncError {
    /// The message.
    message: String,
}

/// `IndexingProgress`: cloned repositories not in the text search index yet.
#[derive(SimpleObject)]
pub(crate) struct IndexingProgress {
    /// Cloned repositories without an index yet.
    not_indexed: i32,
    /// Repositories with an index.
    indexed: i32,
}

/// `GitserverDiskThresholdReached`: a repo node is low on disk space.
#[derive(SimpleObject)]
pub(crate) struct GitserverDiskThresholdReached {
    /// The message.
    message: String,
    /// The repo node's address.
    address: String,
    /// Whether the node paused clones.
    clones_paused: bool,
}

/// Why a repo node does not answer.
#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoNodeUnavailableState {
    /// It shut down cleanly (a restart or an upgrade) or is starting: expected back soon.
    Restarting,
    /// It stopped without shutting down cleanly, or has been gone too long to be a restart.
    Down,
}

/// `RepoNodeUnavailable`: a repo node did not answer, so search leaves out its repositories
/// and they are not cloned or updated.
#[derive(SimpleObject)]
pub(crate) struct RepoNodeUnavailable {
    /// The message.
    message: String,
    /// The repo node's address.
    address: String,
    /// Whether the node is restarting or down.
    state: RepoNodeUnavailableState,
}

/// "45 s", "3 min", "2 h".
fn age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..120 => format!("{secs} s"),
        120..7200 => format!("{} min", secs / 60),
        _ => format!("{} h", secs / 3600),
    }
}

/// The message for repo node `addr`, unreachable for `reason`.
fn unavailable_message(
    addr: &str,
    reason: &str,
    availability: RepoNodeAvailability,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let ago = |t: chrono::DateTime<chrono::Utc>| age(now.signed_duration_since(t).num_seconds());
    let effect = "Search leaves out the repositories on it, and they are not cloned or updated, \
                  until it is back.";
    match availability {
        RepoNodeAvailability::Stopped { since } => format!(
            "Repo node {addr:?} is restarting: it shut down cleanly {} ago, as for an upgrade. \
             {effect} This usually takes a few minutes.",
            ago(since)
        ),
        RepoNodeAvailability::Starting { since } => format!(
            "Repo node {addr:?} is starting: it started {} ago and does not serve yet. {effect} \
             This usually takes a few minutes.",
            ago(since)
        ),
        RepoNodeAvailability::Down { last_seen } => {
            let seen = match last_seen {
                Some(t) => format!("its last heartbeat was {} ago", ago(t)),
                None => "it has never sent a heartbeat".to_owned(),
            };
            format!(
                "Repo node {addr:?} is down: it is not answering ({reason}), did not shut down \
                 cleanly, and {seen}. {effect} Check the node's pod or host."
            )
        }
    }
}

/// `DeclaredConfigError`: a configuration file could not be (fully) applied; what it applied
/// before stays.
#[derive(SimpleObject)]
pub(crate) struct DeclaredConfigError {
    /// The message.
    message: String,
    /// `resources` or `global_settings`.
    source: String,
    /// The file's path in the worker.
    path: String,
}

/// `StatusMessage`.
#[derive(Union)]
pub(crate) enum StatusMessage {
    DeclaredConfigError(DeclaredConfigError),
    GitUpdatesDisabled(GitUpdatesDisabled),
    NoRepositoriesDetected(NoRepositoriesDetected),
    CloningProgress(CloningProgress),
    ExternalServiceSyncError(ExternalServiceSyncError),
    SyncError(SyncError),
    IndexingProgress(IndexingProgress),
    GitserverDiskThresholdReached(GitserverDiskThresholdReached),
    RepoNodeUnavailable(RepoNodeUnavailable),
}

/// `n` with thousands separators (`1,413`).
pub(crate) fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1 repository" / "2 repositories".
fn repositories(n: i64) -> String {
    let noun = if n == 1 { "repository" } else { "repositories" };
    format!("{} {noun}", thousands(n))
}

/// What a configured repo node answered about its disk.
enum NodeDisk {
    /// Percent of the disk in use, when the node knows its disk size.
    Used(Option<f64>),
    /// The node could not be reached, failed, or did not answer in time.
    Unavailable(String),
}

/// Asks every configured repo node for its disk usage.
async fn node_disks(ctx: &Context<'_>) -> Result<Vec<(String, NodeDisk)>> {
    let clients = services(ctx)?.repo_clients.clone();
    let nodes: Vec<String> = clients.router().nodes().map(str::to_owned).collect();
    let tasks: Vec<_> = nodes
        .into_iter()
        .map(|addr| {
            let clients = clients.clone();
            tokio::spawn(async move {
                let disk = match clients.for_node(&addr) {
                    Err(e) => NodeDisk::Unavailable(e.to_string()),
                    Ok(mut c) => match tokio::time::timeout(
                        DISK_INFO_TIMEOUT,
                        c.disk_info(pb::DiskInfoRequest {}),
                    )
                    .await
                    {
                        Err(_) => NodeDisk::Unavailable(format!(
                            "no answer within {} s",
                            DISK_INFO_TIMEOUT.as_secs()
                        )),
                        Ok(Err(status)) => NodeDisk::Unavailable(unavailable_reason(&status)),
                        Ok(Ok(info)) => {
                            let info = info.into_inner();
                            NodeDisk::Used((info.total_bytes > 0).then(|| {
                                let used = info.total_bytes.saturating_sub(info.free_bytes);
                                used as f64 * 100.0 / info.total_bytes as f64
                            }))
                        }
                    },
                };
                (addr, disk)
            })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(d) = t.await {
            out.push(d);
        }
    }
    Ok(out)
}

/// A short reason for a failed call to a repo node.
fn unavailable_reason(status: &sourcegull_rpc::tonic::Status) -> String {
    let message = status.message().trim();
    if message.is_empty() {
        format!("{:?}", status.code())
    } else {
        message.to_owned()
    }
}

/// The status message queries.
#[derive(Default)]
pub struct StatusMessagesQuery;

#[Object]
impl StatusMessagesQuery {
    /// FOR INTERNAL USE ONLY: Lists all status messages (site admins only).
    async fn status_messages(&self, ctx: &Context<'_>) -> Result<Vec<StatusMessage>> {
        require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let mut messages = Vec::new();

        for file in svc.db.declared().sources().await.map_err(db_error)? {
            if let Some(error) = file.error {
                let var = match file.source.as_str() {
                    sourcegull_db::GLOBAL_SETTINGS_SOURCE => "SOURCEGULL_GLOBAL_SETTINGS_FILE",
                    _ => "SOURCEGULL_RESOURCES_FILE",
                };
                messages.push(StatusMessage::DeclaredConfigError(DeclaredConfigError {
                    message: format!("{var} ({}) could not be applied: {error}", file.path),
                    source: file.source,
                    path: file.path,
                }));
            }
        }

        if svc.config.load().value.disable_auto_git_updates {
            messages.push(StatusMessage::GitUpdatesDisabled(GitUpdatesDisabled {
                message: "Repositories will not be cloned or updated.".into(),
            }));
        }

        let stats = svc
            .db
            .search_index()
            .repository_stats()
            .await
            .map_err(db_error)?;
        let services_list = svc
            .db
            .external_services(None)
            .list(None, 1000, 0)
            .await
            .map_err(db_error)?;
        let sync_errors: Vec<_> = services_list
            .into_iter()
            .filter(|s| s.last_sync_error.as_deref().is_some_and(|e| !e.is_empty()))
            .collect();
        if stats.total == 0 && sync_errors.is_empty() {
            messages.push(StatusMessage::NoRepositoriesDetected(
                NoRepositoriesDetected {
                    message: "No repositories have been added yet.".into(),
                },
            ));
        }
        for s in sync_errors {
            messages.push(StatusMessage::ExternalServiceSyncError(
                ExternalServiceSyncError {
                    message: s.last_sync_error.clone().unwrap_or_default(),
                    service: s,
                },
            ));
        }

        // Repo nodes that paused clones, then nodes that do not answer or are over the disk usage
        // threshold.
        let paused = svc
            .db
            .search_index()
            .clones_paused()
            .await
            .map_err(db_error)?;
        let paused_total: i64 = paused.iter().map(|(_, n)| n).sum();
        for (node, n) in &paused {
            let node_name = if node.is_empty() {
                "(unassigned)"
            } else {
                node
            };
            messages.push(StatusMessage::GitserverDiskThresholdReached(
                GitserverDiskThresholdReached {
                    message: format!(
                        "Clones are paused on repo node {node_name:?}: it is low on disk space. \
                         {} waiting for their first clone.",
                        repositories(*n)
                    ),
                    address: node.clone(),
                    clones_paused: true,
                },
            ));
        }
        let disks = node_disks(ctx).await?;
        let heartbeats = if disks
            .iter()
            .any(|(_, d)| matches!(d, NodeDisk::Unavailable(_)))
        {
            let store = svc.db.repo_moves();
            Some((
                store.heartbeats().await.map_err(db_error)?,
                store.now().await.map_err(db_error)?,
            ))
        } else {
            None
        };
        for (addr, disk) in disks {
            let percent = match disk {
                NodeDisk::Unavailable(reason) => {
                    let (beats, now) = match &heartbeats {
                        Some((beats, now)) => (beats.as_slice(), *now),
                        None => (&[][..], chrono::Utc::now()),
                    };
                    let availability = sourcegull_db::repo_node_availability(beats, &addr, now);
                    messages.push(StatusMessage::RepoNodeUnavailable(RepoNodeUnavailable {
                        message: unavailable_message(&addr, &reason, availability, now),
                        state: if availability.is_temporary() {
                            RepoNodeUnavailableState::Restarting
                        } else {
                            RepoNodeUnavailableState::Down
                        },
                        address: addr,
                    }));
                    continue;
                }
                NodeDisk::Used(None) => continue,
                NodeDisk::Used(Some(p)) => p,
            };
            if percent >= DISK_WARNING_PERCENT && !paused.iter().any(|(n, _)| *n == addr) {
                messages.push(StatusMessage::GitserverDiskThresholdReached(
                    GitserverDiskThresholdReached {
                        message: format!(
                            "The disk usage on repo node {addr:?} is over {DISK_WARNING_PERCENT}% \
                             ({percent:.2}% used)."
                        ),
                        address: addr,
                        clones_paused: false,
                    },
                ));
            }
        }

        // Paused clones have their own message above.
        let failed = stats.failed_fetch - paused_total;
        if failed > 0 {
            messages.push(StatusMessage::SyncError(SyncError {
                message: format!(
                    "{} failed last attempt to sync content from code host",
                    repositories(failed)
                ),
            }));
        }

        if stats.queued + stats.cloning > 0 {
            let mut sentences = Vec::new();
            if stats.queued > 0 {
                sentences.push(format!(
                    "{} enqueued for cloning.",
                    repositories(stats.queued)
                ));
            }
            if stats.cloning > 0 {
                sentences.push(format!(
                    "{} currently cloning...",
                    repositories(stats.cloning)
                ));
            }
            messages.push(StatusMessage::CloningProgress(CloningProgress {
                message: sentences.join(" "),
                cloned: to_int(stats.cloned),
                cloning: to_int(stats.cloning),
                queued: to_int(stats.queued),
                total: to_int(stats.total),
            }));
        }

        if stats.not_indexed > 0 {
            messages.push(StatusMessage::IndexingProgress(IndexingProgress {
                not_indexed: to_int(stats.not_indexed),
                indexed: to_int(stats.indexed),
            }));
        }

        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1413), "1,413");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(-1000), "-1,000");
        assert_eq!(repositories(1), "1 repository");
        assert_eq!(repositories(1426), "1,426 repositories");
    }
}
