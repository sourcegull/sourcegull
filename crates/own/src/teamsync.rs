//! Teams synced from GitLab groups (`own.syncTeamsFromGitLabGroups`).
//!
//! For every live GitLab connection, each group the connection's token can see becomes a
//! read-only team named after the group's full path (`acme/backend`, the handle CODEOWNERS
//! files use), with the group's name as display name. Nested groups become child teams of
//! their parent group's team. A team's members are the group's direct members that are
//! linked to Sourcegull users through their GitLab external accounts (same instance, same GitLab
//! user id); unlinked members are skipped.
//!
//! Synced teams remember their connection and group id (`teams.external_service_id`,
//! `external_id`), so renamed groups rename their teams and groups that disappear (or whose
//! connection was deleted) delete their teams. A team made in Sourcegull with a group's name is
//! adopted by the sync. A group whose name is taken by a user, an organization or a team of
//! another connection is skipped with a warning.
//!
//! [`run_team_sync`] (feature `jobs`, a leader-elected singleton in the `worker` role) runs the
//! sync every [`SYNC_INTERVAL`] while the site setting is on; `syncTeamsFromGitLabGroups` (a
//! GraphQL mutation for site admins) runs it at once. The background sync writes under its
//! lease [`Fence`]: each connection's changes are one transaction, refused once another
//! process took the lease.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use sourcegull_codehost::{CodeHostGroup, GroupSource};
use sourcegull_crypto::Keyring;
use sourcegull_db::{Db, DbError, Fence};
use sourcegull_repos::Registry;
use sourcegull_types::{ExternalServiceKind, UserId};
use sqlx::{PgConnection, PgPool, Row};

use crate::store::{set_members, valid_team_name};

/// How often the background sync runs.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Lease name of the background sync.
pub const TEAM_SYNC: &str = "own-team-sync";

/// A failed sync.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    #[error("code host connection {connection}: {message}")]
    CodeHost { connection: i64, message: String },
}

impl From<sqlx::Error> for SyncError {
    fn from(e: sqlx::Error) -> Self {
        SyncError::Db(e.into())
    }
}

/// What a sync changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Teams created (or adopted).
    pub created: u64,
    /// Teams whose name, display name or parent changed.
    pub updated: u64,
    /// Teams deleted because their group or connection is gone.
    pub deleted: u64,
    /// Groups skipped (invalid or taken names).
    pub skipped: u64,
    /// Memberships of synced teams after the sync.
    pub members: u64,
    /// Problems worth showing an admin.
    pub warnings: Vec<String>,
}

/// The group source of one code host connection.
#[derive(Clone)]
pub struct ConnectionGroups {
    /// The connection (`external_services.id`).
    pub external_service_id: i64,
    pub source: Arc<dyn GroupSource>,
}

/// Builds the group sources of every live GitLab connection. Connections with an unusable
/// configuration are reported in the returned warnings and keep their teams.
pub async fn connection_sources(
    db: &Db,
    keyring: Option<Arc<Keyring>>,
    registry: &Registry,
) -> Result<(Vec<ConnectionGroups>, Vec<String>), SyncError> {
    let services = db
        .external_services(keyring)
        .list_with_config(ExternalServiceKind::GitLab)
        .await?;
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (svc, text) in services {
        match registry
            .for_connection(svc.id.get())
            .group_source(svc.kind, &text)
        {
            Ok(source) => out.push(ConnectionGroups {
                external_service_id: svc.id.get(),
                source,
            }),
            Err(e) => warnings.push(format!(
                "connection {} ({}): {e}",
                svc.id.get(),
                svc.display_name
            )),
        }
    }
    Ok((out, warnings))
}

/// Syncs the teams of every live GitLab connection: builds the sources, syncs each, and
/// deletes the teams of connections that no longer exist. Writes only while `fence` is
/// current (a [`DbError::Fenced`] stops the sync).
pub async fn sync_all(
    db: &Db,
    keyring: Option<Arc<Keyring>>,
    registry: &Registry,
    fence: &Fence,
) -> Result<SyncStats, SyncError> {
    let all_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM external_services WHERE deleted_at IS NULL AND kind = 'GITLAB'",
    )
    .fetch_all(db.pool())
    .await?;
    let (sources, warnings) = connection_sources(db, keyring, registry).await?;
    let mut stats = SyncStats {
        warnings,
        ..Default::default()
    };
    for c in &sources {
        match sync_connection(db, c, fence).await {
            Ok(s) => stats.add(s),
            Err(SyncError::Db(e)) if e.is_fenced() => return Err(e.into()),
            Err(e) => stats.warnings.push(e.to_string()),
        }
    }
    stats.deleted += delete_orphans(db.pool(), &all_ids, fence).await?;
    Ok(stats)
}

impl SyncStats {
    fn add(&mut self, other: SyncStats) {
        self.created += other.created;
        self.updated += other.updated;
        self.deleted += other.deleted;
        self.skipped += other.skipped;
        self.members += other.members;
        self.warnings.extend(other.warnings);
    }
}

/// Deletes synced teams whose connection is not in `live` (deleted connections).
async fn delete_orphans(pool: &PgPool, live: &[i64], fence: &Fence) -> Result<u64, SyncError> {
    let mut tx = fence.begin(pool).await?;
    let n = sqlx::query(
        "DELETE FROM teams WHERE external_id IS NOT NULL \
           AND (external_service_id IS NULL OR NOT (external_service_id = ANY($1)))",
    )
    .bind(live)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// The depth of a group path (top-level groups first, so parents exist before children).
fn depth(g: &CodeHostGroup) -> usize {
    g.full_path.matches('/').count()
}

/// Syncs the teams of one connection with its groups, writing in one transaction while
/// `fence` is current.
pub async fn sync_connection(
    db: &Db,
    conn: &ConnectionGroups,
    fence: &Fence,
) -> Result<SyncStats, SyncError> {
    let code_host = |e: sourcegull_codehost::CodeHostError| SyncError::CodeHost {
        connection: conn.external_service_id,
        message: e.to_string(),
    };
    let pool = db.pool();
    let mut groups = conn.source.list_groups().await.map_err(code_host)?;
    groups.sort_by(|a, b| depth(a).cmp(&depth(b)).then(a.full_path.cmp(&b.full_path)));
    // Members first (network), then the database changes.
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for g in &groups {
        let m = conn.source.group_members(&g.id).await.map_err(code_host)?;
        members.insert(g.id.clone(), m.into_iter().map(|m| m.account_id).collect());
    }
    let accounts: Vec<String> = members
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let users: HashMap<String, UserId> = sqlx::query(
        "SELECT a.account_id, a.user_id FROM user_external_accounts a \
         JOIN users u ON u.id = a.user_id \
         WHERE a.service_type = $1 AND a.service_id = $2 AND a.account_id = ANY($3) \
           AND a.deleted_at IS NULL AND u.deleted_at IS NULL",
    )
    .bind(conn.source.service_type())
    .bind(conn.source.service_id())
    .bind(&accounts)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| -> sqlx::Result<_> { Ok((r.try_get("account_id")?, UserId(r.try_get("user_id")?))) })
    .collect::<sqlx::Result<_>>()?;

    let svc = conn.external_service_id;
    let existing: BTreeMap<String, (i64, String, Option<String>, Option<i64>)> = sqlx::query(
        "SELECT id, external_id, name, display_name, parent_team_id FROM teams \
         WHERE external_service_id = $1",
    )
    .bind(svc)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| -> sqlx::Result<_> {
        Ok((
            r.try_get::<String, _>("external_id")?,
            (
                r.try_get("id")?,
                r.try_get("name")?,
                r.try_get("display_name")?,
                r.try_get("parent_team_id")?,
            ),
        ))
    })
    .collect::<sqlx::Result<_>>()?;

    let mut tx = fence.begin(pool).await?;
    let mut stats = SyncStats::default();
    // Group id → team id, for parents.
    let mut team_of: HashMap<String, i64> = HashMap::new();
    for g in &groups {
        let parent = g.parent_id.as_ref().and_then(|p| team_of.get(p)).copied();
        let display = Some(g.name.clone()).filter(|n| !n.is_empty());
        let team = match existing.get(&g.id) {
            Some((id, name, display_name, parent_team)) => {
                if name != &g.full_path || display_name != &display || parent_team != &parent {
                    let renamed = sqlx::query(
                        "UPDATE teams SET name = $2, display_name = $3, parent_team_id = $4, \
                             readonly = true, updated_at = now() \
                         WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM teams o \
                             WHERE lower(o.name) = lower($2) AND o.id <> $1)",
                    )
                    .bind(id)
                    .bind(&g.full_path)
                    .bind(&display)
                    .bind(parent)
                    .execute(&mut *tx)
                    .await?;
                    if renamed.rows_affected() == 1 {
                        stats.updated += 1;
                    } else {
                        stats.warnings.push(format!(
                            "group {} was renamed to a name another team has; kept {name:?}",
                            g.full_path
                        ));
                    }
                }
                Some(*id)
            }
            None => {
                adopt_or_create(&mut tx, svc, g, display.as_deref(), parent, &mut stats).await?
            }
        };
        let Some(team) = team else {
            stats.skipped += 1;
            continue;
        };
        team_of.insert(g.id.clone(), team);
        let mut ids: Vec<UserId> = members
            .get(&g.id)
            .into_iter()
            .flatten()
            .filter_map(|a| users.get(a).copied())
            .collect();
        ids.sort_by_key(|u| u.get());
        ids.dedup();
        stats.members += ids.len() as u64;
        set_members(&mut tx, team, &ids).await?;
    }
    // Groups that are gone (or skipped since) lose their teams.
    let keep: Vec<String> = team_of.keys().cloned().collect();
    stats.deleted += sqlx::query(
        "DELETE FROM teams WHERE external_service_id = $1 AND NOT (external_id = ANY($2))",
    )
    .bind(svc)
    .bind(&keep)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(stats)
}

/// Creates the team of a group, or adopts the Sourcegull team of the same name, on `tx` (the
/// sync's transaction). `None` (with a warning) when the name is invalid or taken.
async fn adopt_or_create(
    tx: &mut PgConnection,
    svc: i64,
    g: &CodeHostGroup,
    display: Option<&str>,
    parent: Option<i64>,
    stats: &mut SyncStats,
) -> Result<Option<i64>, SyncError> {
    if !valid_team_name(&g.full_path) {
        stats
            .warnings
            .push(format!("group {:?} is not a valid team name", g.full_path));
        return Ok(None);
    }
    let same_name: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, external_service_id FROM teams WHERE lower(name) = lower($1) FOR UPDATE",
    )
    .bind(&g.full_path)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match same_name {
        Some((id, None)) => {
            sqlx::query(
                "UPDATE teams SET external_service_id = $2, external_id = $3, display_name = $4, \
                     parent_team_id = $5, readonly = true, updated_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(svc)
            .bind(&g.id)
            .bind(display)
            .bind(parent)
            .execute(&mut *tx)
            .await?;
            id
        }
        Some((_, Some(other))) => {
            stats.warnings.push(format!(
                "group {} has the name of a team synced from connection {other}",
                g.full_path
            ));
            return Ok(None);
        }
        None => {
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM users WHERE lower(username) = lower($1) \
                     AND deleted_at IS NULL) \
                 OR EXISTS (SELECT 1 FROM orgs WHERE lower(name) = lower($1) \
                     AND deleted_at IS NULL)",
            )
            .bind(&g.full_path)
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                stats.warnings.push(format!(
                    "group {} has the name of a user or an organization",
                    g.full_path
                ));
                return Ok(None);
            }
            sqlx::query_scalar(
                "INSERT INTO teams (name, display_name, readonly, parent_team_id, \
                     external_service_id, external_id) \
                 VALUES ($1, $2, true, $3, $4, $5) RETURNING id",
            )
            .bind(&g.full_path)
            .bind(display)
            .bind(parent)
            .bind(svc)
            .bind(&g.id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    stats.created += 1;
    Ok(Some(id))
}

/// Runs the sync every [`SYNC_INTERVAL`] while `own.syncTeamsFromGitLabGroups` is on (the
/// setting is checked every minute, so turning it on syncs right away), fenced by `fence`,
/// until `cancel` fires.
#[cfg(feature = "jobs")]
pub async fn run_team_sync(
    db: Db,
    keyring: Option<Arc<Keyring>>,
    config: sourcegull_config::SiteConfigHandle,
    fence: Fence,
    cancel: tokio_util::sync::CancellationToken,
) {
    let registry = Registry::default();
    let mut last: Option<std::time::Instant> = None;
    loop {
        let enabled = config.load().value.own_sync_teams_from_gitlab_groups;
        if !enabled {
            last = None;
        } else if last.is_none_or(|t| t.elapsed() >= SYNC_INTERVAL) {
            last = Some(std::time::Instant::now());
            match sync_all(&db, keyring.clone(), &registry, &fence).await {
                Ok(s) => {
                    for w in &s.warnings {
                        tracing::warn!(warning = %w, "team sync");
                    }
                    tracing::info!(
                        created = s.created,
                        updated = s.updated,
                        deleted = s.deleted,
                        skipped = s.skipped,
                        members = s.members,
                        "synced teams from GitLab groups"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "syncing teams from GitLab groups failed"),
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
    }
}
