//! Repository permissions: `user_repo_permissions`, its materialized
//! per-user bitmaps, explicit permissions for users who do not exist yet, and per-repo state.
//!
//! Who may read a repository is decided by the SQL function `repo_restricted()` (unrestricted
//! repositories are readable by everyone) plus the `user_repo_permissions` rows. Every write
//! here rewrites the affected users' `user_perm_bitmaps` rows in the same transaction and sends
//! `NOTIFY authz` (see [`authz_payload`]) so web nodes drop their cached copies.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use roaring::RoaringBitmap;
use sourcegull_types::{RepoId, UserId};
use sqlx::{PgConnection, PgPool};

use crate::notify::{channels, notify};
use crate::{DbError, Result};

/// Above this many affected users one `all` notification replaces the per-user ones.
const NOTIFY_PER_USER_MAX: usize = 100;

/// Where a `user_repo_permissions` row comes from. Each source replaces only its own rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermsSource {
    /// The explicit permissions API (`setRepositoryPermissionsForUsers`).
    Explicit,
    /// The code host permissions sync.
    Sync,
}

impl PermsSource {
    /// The stored form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Sync => "sync",
        }
    }
}

/// How explicit permissions name users (`permissions.userMapping.bindID`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    Username,
    Email,
}

impl BindKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Email => "email",
        }
    }
}

/// Payloads of `NOTIFY authz`.
pub mod authz_payload {
    /// The unrestricted set may have changed.
    pub const UNRESTRICTED: &str = "unrestricted";
    /// Any user's grants may have changed.
    pub const ALL: &str = "all";
    /// Prefix of `user:<id>`: that user's grants changed.
    pub const USER_PREFIX: &str = "user:";
}

/// Grants added and removed by a write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PermsChange {
    pub added: usize,
    pub removed: usize,
}

/// A repository's permission state, for the admin API.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoPermsInfo {
    pub repo_id: RepoId,
    pub unrestricted: bool,
    pub synced_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// Why one user can or cannot read one repository (the inputs of `repo_restricted()` plus the
/// user's grants), for the admin "why can X see Y" explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessFacts {
    /// The repository is not visible to everyone on the code host.
    pub private: bool,
    /// The code host type of the repository (`gitlab`, `github`, `gitea`).
    pub service_type: String,
    /// Marked unrestricted with `setRepositoryPermissionsUnrestricted`.
    pub marked_unrestricted: bool,
    /// Live connections without an `authorization` block that list the repository, as
    /// `(id, display name)`.
    pub unrestricted_connections: Vec<(i64, String)>,
    /// The user's grants of the repository, as `(source, granted at)`.
    pub grants: Vec<(String, DateTime<Utc>)>,
    /// When the repository's code host permissions were last synced (repo-centric).
    pub repo_synced_at: Option<DateTime<Utc>>,
}

/// `private`, `external_service_type`, `unrestricted` and `synced_at` of a repository.
type FactsRow = (bool, String, Option<bool>, Option<DateTime<Utc>>);

/// Reads and writes repository permissions.
#[derive(Debug, Clone)]
pub struct PermsStore {
    pool: PgPool,
}

impl PermsStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Live repositories readable by everyone, given `permissions.userMapping.enabled`.
    pub async fn unrestricted_repos(&self, user_mapping: bool) -> Result<RoaringBitmap> {
        let ids: Vec<RepoId> = sqlx::query_scalar(
            "SELECT id FROM repo
             WHERE deleted_at IS NULL AND NOT repo_restricted(id, private, $1)",
        )
        .bind(user_mapping)
        .fetch_all(&self.pool)
        .await?;
        Ok(to_bitmap(&ids))
    }

    /// The repositories granted to `user` (from the materialized bitmap; empty without one).
    pub async fn user_bitmap(&self, user: UserId) -> Result<RoaringBitmap> {
        let bytes: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT bitmap FROM user_perm_bitmaps WHERE user_id = $1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await?;
        match bytes {
            None => Ok(RoaringBitmap::new()),
            Some(b) => RoaringBitmap::deserialize_from(&b[..])
                .map_err(|_| DbError::Invalid("corrupt user_perm_bitmaps row")),
        }
    }

    /// Replaces the `source` grants of `repo` with `users`, and (for explicit permissions) the
    /// repo's pending permissions with `pending` bind ids of `kind`, in one transaction.
    /// Bind ids are lowercased. Marks the repo's permissions updated.
    pub async fn set_repo_perms(
        &self,
        repo: RepoId,
        source: PermsSource,
        users: &[UserId],
        pending: Option<(BindKind, &[String])>,
    ) -> Result<PermsChange> {
        let mut tx = self.pool.begin().await?;
        let users: Vec<UserId> = users
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let removed: Vec<UserId> = sqlx::query_scalar(
            "DELETE FROM user_repo_permissions
             WHERE repo_id = $1 AND source = $2 AND NOT (user_id = ANY($3))
             RETURNING user_id",
        )
        .bind(repo)
        .bind(source.as_str())
        .bind(&users)
        .fetch_all(&mut *tx)
        .await?;
        let added: Vec<UserId> = sqlx::query_scalar(
            "INSERT INTO user_repo_permissions (user_id, repo_id, source)
             SELECT u, $1, $2 FROM unnest($3::bigint[]) AS t(u)
             ON CONFLICT DO NOTHING
             RETURNING user_id",
        )
        .bind(repo)
        .bind(source.as_str())
        .bind(&users)
        .fetch_all(&mut *tx)
        .await?;
        if let Some((kind, ids)) = pending {
            let ids: Vec<String> = ids
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            sqlx::query("DELETE FROM user_pending_permissions WHERE repo_id = $1")
                .bind(repo)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO user_pending_permissions (bind_kind, bind_id, repo_id)
                 SELECT $1, b, $2 FROM unnest($3::text[]) AS t(b)",
            )
            .bind(kind.as_str())
            .bind(repo)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        }
        touch_repo(&mut tx, repo, source == PermsSource::Sync).await?;
        let change = PermsChange {
            added: added.len(),
            removed: removed.len(),
        };
        let changed: Vec<UserId> = removed.into_iter().chain(added).collect();
        rebuild_bitmaps(&mut tx, &changed).await?;
        tx.commit().await?;
        Ok(change)
    }

    /// Replaces `user`'s synced grants on the repositories of code host instance `service_id`
    /// (`repo.external_service_id`) with `repos` (user-centric sync). Grants on other code
    /// hosts and explicit grants are kept.
    pub async fn set_user_sync_perms(
        &self,
        user: UserId,
        service_id: &str,
        repos: &[RepoId],
    ) -> Result<PermsChange> {
        let repos: Vec<RepoId> = repos
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut tx = self.pool.begin().await?;
        let removed = sqlx::query(
            "DELETE FROM user_repo_permissions p USING repo r
             WHERE p.user_id = $1 AND p.source = 'sync' AND r.id = p.repo_id
               AND r.external_service_id = $2 AND NOT (p.repo_id = ANY($3))",
        )
        .bind(user)
        .bind(service_id)
        .bind(&repos)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let added = sqlx::query(
            "INSERT INTO user_repo_permissions (user_id, repo_id, source)
             SELECT $1, r.id, 'sync' FROM repo r
             WHERE r.id = ANY($3) AND r.external_service_id = $2
             ON CONFLICT DO NOTHING",
        )
        .bind(user)
        .bind(service_id)
        .bind(&repos)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if added + removed > 0 {
            rebuild_bitmaps(&mut tx, &[user]).await?;
        }
        tx.commit().await?;
        Ok(PermsChange {
            added: usize::try_from(added).unwrap_or(usize::MAX),
            removed: usize::try_from(removed).unwrap_or(usize::MAX),
        })
    }

    /// Live repositories of code host instance `service_id` with these external ids.
    pub async fn repos_by_external_ids(
        &self,
        service_type: &str,
        service_id: &str,
        external_ids: &[String],
    ) -> Result<Vec<RepoId>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM repo
             WHERE external_service_type = $1 AND external_service_id = $2
               AND external_id = ANY($3) AND deleted_at IS NULL
             ORDER BY id",
        )
        .bind(service_type)
        .bind(service_id)
        .bind(external_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks `repos` readable by everyone, or not.
    pub async fn set_unrestricted(&self, repos: &[RepoId], unrestricted: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO repo_permissions (repo_id, unrestricted)
             SELECT r, $2 FROM unnest($1::int[]) AS t(r)
             ON CONFLICT (repo_id) DO UPDATE
                 SET unrestricted = EXCLUDED.unrestricted, updated_at = now()",
        )
        .bind(repos)
        .bind(unrestricted)
        .execute(&mut *tx)
        .await?;
        notify(&mut *tx, channels::AUTHZ, authz_payload::UNRESTRICTED).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The permission state of `repo`, if any was ever recorded.
    pub async fn repo_info(&self, repo: RepoId) -> Result<Option<RepoPermsInfo>> {
        Ok(sqlx::query_as(
            "SELECT repo_id, unrestricted, synced_at, updated_at
             FROM repo_permissions WHERE repo_id = $1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// What decides whether `user` may read `repo`; `None` when the repository does not exist.
    pub async fn access_facts(&self, user: UserId, repo: RepoId) -> Result<Option<AccessFacts>> {
        let row: Option<FactsRow> = sqlx::query_as(
            "SELECT r.private, r.external_service_type, rp.unrestricted, rp.synced_at
             FROM repo r
             LEFT JOIN repo_permissions rp ON rp.repo_id = r.id
             WHERE r.id = $1 AND r.deleted_at IS NULL",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        let Some((private, service_type, marked, repo_synced_at)) = row else {
            return Ok(None);
        };
        let unrestricted_connections = sqlx::query_as(
            "SELECT es.id, es.display_name FROM external_service_repos esr
             JOIN external_services es ON es.id = esr.external_service_id
             WHERE esr.repo_id = $1 AND es.unrestricted AND es.deleted_at IS NULL
             ORDER BY es.id",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let grants = sqlx::query_as(
            "SELECT source, created_at FROM user_repo_permissions
             WHERE user_id = $1 AND repo_id = $2 ORDER BY source",
        )
        .bind(user)
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(AccessFacts {
            private,
            service_type,
            marked_unrestricted: marked.unwrap_or(false),
            unrestricted_connections,
            grants,
            repo_synced_at,
        }))
    }

    /// Live users granted `repo` (any source), by id, with the total count.
    pub async fn repo_users(
        &self,
        repo: RepoId,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<UserId>, i64)> {
        let ids = sqlx::query_scalar(
            "SELECT DISTINCT p.user_id FROM user_repo_permissions p
             JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
             WHERE p.repo_id = $1 ORDER BY p.user_id LIMIT $2 OFFSET $3",
        )
        .bind(repo)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total = sqlx::query_scalar(
            "SELECT count(DISTINCT p.user_id) FROM user_repo_permissions p
             JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
             WHERE p.repo_id = $1",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;
        Ok((ids, total))
    }

    /// Repositories granted to `user` (any source), by id.
    pub async fn user_repos(&self, user: UserId) -> Result<Vec<RepoId>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT repo_id FROM user_repo_permissions WHERE user_id = $1 ORDER BY repo_id",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Repositories waiting for the user named `bind_id` (lowercased) of `kind`.
    pub async fn pending_repos(&self, kind: BindKind, bind_id: &str) -> Result<Vec<RepoId>> {
        Ok(sqlx::query_scalar(
            "SELECT repo_id FROM user_pending_permissions
             WHERE bind_kind = $1 AND bind_id = $2 ORDER BY repo_id",
        )
        .bind(kind.as_str())
        .bind(bind_id.trim().to_lowercase())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Bind ids of `kind` with pending permissions, sorted.
    pub async fn pending_bind_ids(&self, kind: BindKind) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT bind_id FROM user_pending_permissions
             WHERE bind_kind = $1 ORDER BY bind_id",
        )
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?)
    }
}

/// Turns the pending permissions waiting for `bind_id` of `kind` into explicit grants of
/// `user`. Called in the transaction that creates the user or verifies the email.
pub(crate) async fn grant_pending(
    tx: &mut PgConnection,
    user: UserId,
    kind: BindKind,
    bind_id: &str,
) -> Result<()> {
    let granted: Vec<RepoId> = sqlx::query_scalar(
        "WITH taken AS (
             DELETE FROM user_pending_permissions
             WHERE bind_kind = $1 AND bind_id = $2
             RETURNING repo_id)
         INSERT INTO user_repo_permissions (user_id, repo_id, source)
         SELECT $3, repo_id, 'explicit' FROM taken
         ON CONFLICT DO NOTHING
         RETURNING repo_id",
    )
    .bind(kind.as_str())
    .bind(bind_id.trim().to_lowercase())
    .bind(user)
    .fetch_all(&mut *tx)
    .await?;
    if !granted.is_empty() {
        rebuild_bitmaps(tx, &[user]).await?;
    }
    Ok(())
}

async fn touch_repo(tx: &mut PgConnection, repo: RepoId, synced: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO repo_permissions (repo_id, synced_at) VALUES ($1, CASE WHEN $2 THEN now() END)
         ON CONFLICT (repo_id) DO UPDATE SET updated_at = now(),
             synced_at = CASE WHEN $2 THEN now() ELSE repo_permissions.synced_at END",
    )
    .bind(repo)
    .bind(synced)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Rewrites the bitmaps of `users` from their `user_repo_permissions` rows and notifies.
pub(crate) async fn rebuild_bitmaps(tx: &mut PgConnection, users: &[UserId]) -> Result<()> {
    let users: Vec<UserId> = users
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if users.is_empty() {
        return Ok(());
    }
    let rows: Vec<(UserId, Vec<RepoId>)> = sqlx::query_as(
        "SELECT u, coalesce(array_agg(DISTINCT p.repo_id) FILTER (WHERE p.repo_id IS NOT NULL), '{}')
         FROM unnest($1::bigint[]) AS t(u)
         LEFT JOIN user_repo_permissions p ON p.user_id = u
         GROUP BY u",
    )
    .bind(&users)
    .fetch_all(&mut *tx)
    .await?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut blobs = Vec::with_capacity(rows.len());
    for (user, repos) in rows {
        let bitmap = to_bitmap(&repos);
        let mut bytes = Vec::with_capacity(bitmap.serialized_size());
        bitmap
            .serialize_into(&mut bytes)
            .map_err(|_| DbError::Invalid("serializing a permissions bitmap"))?;
        ids.push(user);
        blobs.push(bytes);
    }
    sqlx::query(
        "INSERT INTO user_perm_bitmaps (user_id, bitmap)
         SELECT u, b FROM unnest($1::bigint[], $2::bytea[]) AS t(u, b)
         ON CONFLICT (user_id) DO UPDATE SET bitmap = EXCLUDED.bitmap,
             version = user_perm_bitmaps.version + 1, updated_at = now()",
    )
    .bind(&ids)
    .bind(&blobs)
    .execute(&mut *tx)
    .await?;
    if users.len() > NOTIFY_PER_USER_MAX {
        notify(&mut *tx, channels::AUTHZ, authz_payload::ALL).await?;
    } else {
        for u in &users {
            let payload = format!("{}{}", authz_payload::USER_PREFIX, u.get());
            notify(&mut *tx, channels::AUTHZ, &payload).await?;
        }
    }
    Ok(())
}

fn to_bitmap(ids: &[RepoId]) -> RoaringBitmap {
    ids.iter()
        .filter_map(|id| u32::try_from(id.get()).ok())
        .collect()
}
