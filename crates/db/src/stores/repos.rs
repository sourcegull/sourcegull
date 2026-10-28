//! `repo`, `external_service_repos` and `repo_kvps` stores.
//!
//! Discovery writes repos with [`RepoStore::upsert_listed`] and removes the ones a connection
//! no longer lists with [`RepoStore::unlink_unlisted`]. A repo is matched by its external spec
//! (stable across renames and transfers), and soft-deleted once no connection lists it.

use std::collections::HashSet;

use crate::Result;
use crate::stores::repo_update_jobs::{RepoUpdatePriority, RepoUpdateReason, enqueue_update};
use crate::stores::users::escape_like;
use chrono::{DateTime, Utc};
use sourcegull_types::{CloneStatus, ExternalServiceId, Repo, RepoAccess, RepoId, Visibility};
use sqlx::{PgConnection, PgPool};

const COLUMNS: &str = "id, name, uri, url, description, fork, archived, private, visibility, \
     stars, topics, default_branch, last_activity_at, external_id, external_service_type, \
     external_service_id, shard, blocked, created_at, updated_at, deleted_at";

/// A repo as listed by a code host, for [`RepoStore::upsert_listed`].
#[derive(Debug, Clone, PartialEq)]
pub struct ListedRepo {
    pub name: String,
    pub uri: String,
    pub url: String,
    pub description: String,
    pub fork: bool,
    pub archived: bool,
    pub private: bool,
    pub visibility: Visibility,
    pub stars: i32,
    pub topics: Vec<String>,
    pub default_branch: Option<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub external_id: String,
    pub external_service_type: String,
    pub external_service_id: String,
    /// Host-specific metadata.
    pub metadata: serde_json::Value,
    /// Clone URL without credentials.
    pub clone_url: String,
}

/// What [`RepoStore::upsert_listed`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpsertSummary {
    /// Ids of every repo in the batch, in input order.
    pub ids: Vec<RepoId>,
    /// New repos (and soft-deleted repos that came back).
    pub added: Vec<RepoId>,
    /// Existing repos whose metadata changed (including renames).
    pub modified: Vec<RepoId>,
    /// Existing repos with no change.
    pub unmodified: Vec<RepoId>,
    /// Repos queued for a clone or fetch (new repos, and repos whose `last_activity_at`
    /// moved forward).
    pub fetches_queued: Vec<RepoId>,
    /// Other repos soft-deleted because a listed repo took their name.
    pub displaced: Vec<RepoId>,
}

/// Sort order for [`RepoStore::list`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepoOrder {
    /// By name (case-insensitive), then id.
    #[default]
    Name,
    /// By creation time, then id.
    CreatedAt,
    /// By size on disk (unknown sizes last), then id.
    Size,
}

/// Filters for [`RepoStore::list`] and [`RepoStore::count`].
#[derive(Debug, Clone)]
pub struct ListReposOptions {
    /// Case-insensitive substring of the name.
    pub query: Option<String>,
    /// Only repos in this clone state.
    pub clone_status: Option<CloneStatus>,
    /// Only repos whose last clone/fetch failed.
    pub failed_fetch: bool,
    /// Only repos their repo node found corrupt.
    pub corrupted: bool,
    /// `Some(true)`: only repos with a text search index; `Some(false)`: only repos without.
    pub indexed: Option<bool>,
    /// Only repos listed by this connection.
    pub external_service_id: Option<ExternalServiceId>,
    /// Only these repos.
    pub ids: Option<Vec<RepoId>>,
    /// Only repos this viewer may read. `None` lists every repo: internal callers
    /// only; anything answering a viewer passes their access.
    pub access: Option<RepoAccess>,
    pub order: RepoOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListReposOptions {
    fn default() -> Self {
        Self {
            query: None,
            clone_status: None,
            failed_fetch: false,
            corrupted: false,
            indexed: None,
            external_service_id: None,
            ids: None,
            access: None,
            order: RepoOrder::Name,
            descending: false,
            limit: 100,
            offset: 0,
        }
    }
}

/// Reads and writes repos. Soft-deleted repos are invisible to lookups.
#[derive(Debug, Clone)]
pub struct RepoStore {
    pool: PgPool,
}

/// What the code host's last listing said about a repo's project repository
/// ([`RepoStore::code_host_hints`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct CodeHostRepoHints {
    /// The project has no repository yet (GitLab `empty_repo`).
    pub empty: bool,
    /// The project's repository feature is disabled (GitLab `repository_access_level`).
    pub repository_disabled: bool,
    /// The token's effective access level to the project, when the code host reported it.
    pub access_level: Option<i64>,
}

impl RepoStore {
    /// Creates a store on `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Looks up a live repo by id.
    pub async fn get(&self, id: RepoId) -> Result<Option<Repo>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repo WHERE id = $1 AND deleted_at IS NULL"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks up a live repo by name, case-insensitively.
    pub async fn get_by_name(&self, name: &str) -> Result<Option<Repo>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repo WHERE lower(name) = lower($1) AND deleted_at IS NULL"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Looks up a live repo by id if `access` may read it; an unreadable repo is `None`,
    /// exactly like a missing one.
    pub async fn get_authorized(&self, id: RepoId, access: &RepoAccess) -> Result<Option<Repo>> {
        let q = format!(
            "SELECT {COLUMNS} FROM repo r WHERE id = $1 AND deleted_at IS NULL AND {READABLE}"
        );
        Ok(bind_access(sqlx::query_as(&q).bind(id), Some(access))
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Looks up a live repo by name, case-insensitively, if `access` may read it.
    pub async fn get_by_name_authorized(
        &self,
        name: &str,
        access: &RepoAccess,
    ) -> Result<Option<Repo>> {
        let q = format!(
            "SELECT {COLUMNS} FROM repo r
             WHERE lower(name) = lower($1) AND deleted_at IS NULL AND {READABLE}"
        );
        Ok(bind_access(sqlx::query_as(&q).bind(name), Some(access))
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The lower-case service types of the repos in `ids` that are not on GitLab. Search uses
    /// them to pick where an already authorized repo's CODEOWNERS file is looked up.
    pub async fn non_gitlab_service_types(&self, ids: &[i32]) -> Result<Vec<(i32, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, external_service_type FROM repo \
             WHERE id = ANY($1) AND external_service_type <> 'gitlab'",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Of `ids`, the live repos `access` may read.
    pub async fn readable_ids(&self, ids: &[RepoId], access: &RepoAccess) -> Result<Vec<RepoId>> {
        if access.is_all() {
            return Ok(sqlx::query_scalar(
                "SELECT id FROM repo WHERE id = ANY($1) AND deleted_at IS NULL",
            )
            .bind(ids)
            .fetch_all(&self.pool)
            .await?);
        }
        let q = format!(
            "SELECT id FROM repo r WHERE id = ANY($1) AND deleted_at IS NULL AND {READABLE}"
        );
        Ok(bind_access(sqlx::query_scalar(&q).bind(ids), Some(access))
            .fetch_all(&self.pool)
            .await?)
    }

    /// Looks up a live repo by its external spec.
    pub async fn get_by_external_id(
        &self,
        service_type: &str,
        service_id: &str,
        external_id: &str,
    ) -> Result<Option<Repo>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repo
             WHERE external_service_type = $1 AND external_service_id = $2 AND external_id = $3
               AND deleted_at IS NULL"
        ))
        .bind(service_type)
        .bind(service_id)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Lists live repos.
    pub async fn list(&self, opts: &ListReposOptions) -> Result<Vec<Repo>> {
        let dir = if opts.descending { "DESC" } else { "ASC" };
        let order = match opts.order {
            RepoOrder::Name => format!("lower(r.name) {dir}, r.id {dir}"),
            RepoOrder::CreatedAt => format!("r.created_at {dir}, r.id {dir}"),
            RepoOrder::Size => format!("gr.repo_size_bytes {dir} NULLS LAST, r.id {dir}"),
        };
        let cols = COLUMNS
            .split(", ")
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let q = format!(
            "SELECT {cols} FROM repo r
             LEFT JOIN gitserver_repos gr ON gr.repo_id = r.id
             WHERE {FILTER}
             ORDER BY {order} LIMIT $11 OFFSET $12"
        );
        Ok(bind_filter(sqlx::query_as(&q), opts)
            .bind(opts.limit)
            .bind(opts.offset)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Counts live repos matching the filters (`limit`/`offset` are ignored).
    pub async fn count(&self, opts: &ListReposOptions) -> Result<i64> {
        let q = format!(
            "SELECT count(*) FROM repo r LEFT JOIN gitserver_repos gr ON gr.repo_id = r.id
             WHERE {FILTER}"
        );
        let (n,): (i64,) = bind_filter(sqlx::query_as(&q), opts)
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Connections listing a repo, with the clone URL each reported.
    pub async fn sources(&self, id: RepoId) -> Result<Vec<(ExternalServiceId, String)>> {
        Ok(sqlx::query_as(
            "SELECT esr.external_service_id, esr.clone_url FROM external_service_repos esr
             JOIN external_services es ON es.id = esr.external_service_id AND es.deleted_at IS NULL
             WHERE esr.repo_id = $1 ORDER BY esr.external_service_id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// What the code host's last listing said about the repo's project repository
    /// (`metadata.emptyRepo`, `repositoryAccessLevel`, `accessLevel`; GitLab), for the repo
    /// node to classify a refused fetch. All unset when the repo does not exist.
    pub async fn code_host_hints(&self, id: RepoId) -> Result<CodeHostRepoHints> {
        Ok(sqlx::query_as(
            "SELECT coalesce(metadata->>'emptyRepo' = 'true', false) AS empty,
                    coalesce(metadata->>'repositoryAccessLevel' = 'disabled', false)
                        AS repository_disabled,
                    CASE WHEN jsonb_typeof(metadata->'accessLevel') = 'number'
                         THEN (metadata->>'accessLevel')::bigint END AS access_level
             FROM repo WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default())
    }

    /// The host-specific metadata of a repo (as last listed), `None` when it does not exist.
    pub async fn metadata(&self, id: RepoId) -> Result<Option<serde_json::Value>> {
        let row: Option<(sqlx::types::Json<serde_json::Value>,)> =
            sqlx::query_as("SELECT metadata FROM repo WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(m,)| m.0))
    }

    /// Inserts or updates repos listed by connection `service` and links them to it, in one
    /// transaction. Matching is by external spec, so a renamed repo keeps its id; a live repo
    /// of another external spec holding a listed repo's name is soft-deleted (the code host
    /// moved the name). New repos get a `gitserver_repos` row and a clone request; repos whose
    /// `last_activity_at` moved forward get a fetch request.
    pub async fn upsert_listed(
        &self,
        service: ExternalServiceId,
        listed: &[ListedRepo],
    ) -> Result<UpsertSummary> {
        let mut tx = self.pool.begin().await?;
        let mut out = UpsertSummary::default();
        for repo in listed {
            upsert_one(&mut tx, service, repo, &mut out).await?;
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Unlinks every repo of `service` not in `keep`, then soft-deletes the unlinked repos no
    /// other connection lists. Returns the number of repos soft-deleted.
    pub async fn unlink_unlisted(
        &self,
        service: ExternalServiceId,
        keep: &[RepoId],
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = unlink_repos(&mut tx, service, keep).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// The repos linked to connection `service` and their external ids.
    pub async fn linked_external_ids(
        &self,
        service: ExternalServiceId,
    ) -> Result<Vec<(RepoId, String)>> {
        Ok(sqlx::query_as(
            "SELECT r.id, r.external_id FROM external_service_repos esr
             JOIN repo r ON r.id = esr.repo_id
             WHERE esr.external_service_id = $1",
        )
        .bind(service)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Live repos without a repo node assignment (`shard IS NULL`), lowest id first.
    pub async fn unassigned(&self, limit: i64) -> Result<Vec<RepoId>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM repo WHERE shard IS NULL AND deleted_at IS NULL ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Persists repo node assignments for repos that have none yet; repos assigned in
    /// the meantime keep theirs. Sends `NOTIFY repo_shards` on commit when anything changed.
    /// Returns the number of repos assigned.
    pub async fn assign_shards(&self, assignments: &[(RepoId, String)]) -> Result<u64> {
        if assignments.is_empty() {
            return Ok(0);
        }
        let (ids, nodes): (Vec<RepoId>, Vec<String>) = assignments.iter().cloned().unzip();
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(
            "UPDATE repo SET shard = a.node
             FROM unnest($1::int[], $2::text[]) AS a(id, node)
             WHERE repo.id = a.id AND repo.shard IS NULL",
        )
        .bind(&ids)
        .bind(&nodes)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n > 0 {
            crate::notify::notify(
                &mut *tx,
                crate::notify::channels::REPO_SHARDS,
                &crate::notify::repo_shards_payload(&ids),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Every live repo's persisted repo node assignment (for the routing table).
    pub async fn shards(&self) -> Result<Vec<(RepoId, String)>> {
        Ok(sqlx::query_as(
            "SELECT id, shard FROM repo WHERE shard IS NOT NULL AND deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The persisted repo node assignment of each repo in `ids`; `None` for repos that are
    /// unassigned, deleted or missing.
    pub async fn shards_of(&self, ids: &[RepoId]) -> Result<Vec<(RepoId, Option<String>)>> {
        Ok(sqlx::query_as(
            "SELECT a.id, r.shard FROM unnest($1::int[]) AS a(id)
             LEFT JOIN repo r ON r.id = a.id AND r.deleted_at IS NULL",
        )
        .bind(ids)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The repo's key-value pairs, ordered by key.
    pub async fn kvps(&self, id: RepoId) -> Result<Vec<(String, Option<String>)>> {
        Ok(
            sqlx::query_as("SELECT key, value FROM repo_kvps WHERE repo_id = $1 ORDER BY key")
                .bind(id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Every live repo's key-value pairs, ordered by repo and key (for the search catalog).
    pub async fn all_kvps(&self) -> Result<Vec<(RepoId, String, Option<String>)>> {
        Ok(sqlx::query_as(
            "SELECT k.repo_id, k.key, k.value FROM repo_kvps k
             WHERE EXISTS (SELECT 1 FROM repo r WHERE r.id = k.repo_id AND r.deleted_at IS NULL)
             ORDER BY k.repo_id, k.key",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Adds a key-value pair. Returns false when the key already exists.
    pub async fn add_kvp(&self, id: RepoId, key: &str, value: Option<&str>) -> Result<bool> {
        let r = sqlx::query(
            "INSERT INTO repo_kvps (repo_id, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (repo_id, key) DO NOTHING",
        )
        .bind(id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Replaces the value of an existing key. Returns false when the key does not exist.
    pub async fn update_kvp(&self, id: RepoId, key: &str, value: Option<&str>) -> Result<bool> {
        let r = sqlx::query("UPDATE repo_kvps SET value = $3 WHERE repo_id = $1 AND key = $2")
            .bind(id)
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Sets (inserts or replaces) a key-value pair.
    pub async fn set_kvp(&self, id: RepoId, key: &str, value: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO repo_kvps (repo_id, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (repo_id, key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes a key-value pair. Returns whether it existed.
    pub async fn delete_kvp(&self, id: RepoId, key: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM repo_kvps WHERE repo_id = $1 AND key = $2")
            .bind(id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }
}

const FILTER: &str = "r.deleted_at IS NULL
    AND ($1::text IS NULL OR lower(r.name) LIKE $1)
    AND ($2::text IS NULL OR coalesce(gr.clone_status, 'not_cloned') = $2)
    AND (NOT $3 OR gr.last_error IS NOT NULL)
    AND ($4::bigint IS NULL OR EXISTS (
        SELECT 1 FROM external_service_repos esr
        WHERE esr.repo_id = r.id AND esr.external_service_id = $4))
    AND ($5::int[] IS NULL OR r.id = ANY($5))
    AND ($6 OR NOT repo_restricted(r.id, r.private, $7) OR EXISTS (
        SELECT 1 FROM user_repo_permissions p WHERE p.repo_id = r.id AND p.user_id = $8))
    AND (NOT $9 OR gr.corrupted_at IS NOT NULL)
    AND ($10::bool IS NULL OR $10 = EXISTS (
        SELECT 1 FROM search_index_repos s
        WHERE s.repo_id = r.id AND s.index_status = 'indexed'))";

/// Whether the viewer may read `r`: placeholders `$2`/`$3`/`$4` after one leading
/// parameter, bound by [`bind_access`].
const READABLE: &str = "($2 OR NOT repo_restricted(r.id, r.private, $3) OR EXISTS (
    SELECT 1 FROM user_repo_permissions p WHERE p.repo_id = r.id AND p.user_id = $4))";

/// Binds the three parameters of [`READABLE`] (or of the access part of [`FILTER`]):
/// whether every repo is readable, `permissions.userMapping.enabled`, and the viewer.
fn bind_access<'q, Q: BindAccess<'q>>(q: Q, access: Option<&RepoAccess>) -> Q {
    let all = access.is_none_or(RepoAccess::is_all);
    let user_mapping = access.is_some_and(RepoAccess::user_mapping);
    let user = access.and_then(RepoAccess::user_id);
    q.bind_bool(all).bind_bool(user_mapping).bind_user(user)
}

/// The query types [`bind_access`] binds to.
trait BindAccess<'q>: Sized {
    fn bind_bool(self, v: bool) -> Self;
    fn bind_user(self, v: Option<sourcegull_types::UserId>) -> Self;
}

impl<'q, O> BindAccess<'q>
    for sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>
{
    fn bind_bool(self, v: bool) -> Self {
        self.bind(v)
    }
    fn bind_user(self, v: Option<sourcegull_types::UserId>) -> Self {
        self.bind(v)
    }
}

impl<'q, O> BindAccess<'q>
    for sqlx::query::QueryScalar<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>
{
    fn bind_bool(self, v: bool) -> Self {
        self.bind(v)
    }
    fn bind_user(self, v: Option<sourcegull_types::UserId>) -> Self {
        self.bind(v)
    }
}

fn bind_filter<'q, O>(
    q: sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>,
    opts: &'q ListReposOptions,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments> {
    let pattern = opts
        .query
        .as_deref()
        .map(|q| format!("%{}%", escape_like(&q.to_lowercase())));
    let q = q
        .bind(pattern)
        .bind(opts.clone_status.map(CloneStatus::as_str))
        .bind(opts.failed_fetch)
        .bind(opts.external_service_id)
        .bind(opts.ids.as_deref());
    bind_access(q, opts.access.as_ref())
        .bind(opts.corrupted)
        .bind(opts.indexed)
}

#[derive(sqlx::FromRow)]
struct Existing {
    id: RepoId,
    name: String,
    uri: String,
    url: String,
    description: String,
    fork: bool,
    archived: bool,
    private: bool,
    visibility: Visibility,
    stars: i32,
    topics: Vec<String>,
    default_branch: Option<String>,
    last_activity_at: Option<DateTime<Utc>>,
    metadata: serde_json::Value,
    deleted_at: Option<DateTime<Utc>>,
}

impl Existing {
    fn same_as(&self, r: &ListedRepo) -> bool {
        self.name == r.name
            && self.uri == r.uri
            && self.url == r.url
            && self.description == r.description
            && self.fork == r.fork
            && self.archived == r.archived
            && self.private == r.private
            && self.visibility == r.visibility
            && self.stars == r.stars
            && self.topics == r.topics
            && self.default_branch == r.default_branch
            && self.last_activity_at == r.last_activity_at
            && self.metadata == r.metadata
    }
}

async fn upsert_one(
    tx: &mut PgConnection,
    service: ExternalServiceId,
    r: &ListedRepo,
    out: &mut UpsertSummary,
) -> Result<()> {
    let existing: Option<Existing> = sqlx::query_as(
        "SELECT id, name, uri, url, description, fork, archived, private, visibility, stars,
                topics, default_branch, last_activity_at, metadata, deleted_at
         FROM repo
         WHERE external_service_type = $1 AND external_service_id = $2 AND external_id = $3
         FOR UPDATE",
    )
    .bind(&r.external_service_type)
    .bind(&r.external_service_id)
    .bind(&r.external_id)
    .fetch_optional(&mut *tx)
    .await?;

    // Free the name if another live repo holds it.
    let displaced: Vec<RepoId> = sqlx::query_scalar(
        "UPDATE repo SET deleted_at = now(), updated_at = now()
         WHERE lower(name) = lower($1) AND deleted_at IS NULL AND id <> coalesce($2, -1)
         RETURNING id",
    )
    .bind(&r.name)
    .bind(existing.as_ref().map(|e| e.id))
    .fetch_all(&mut *tx)
    .await?;
    out.displaced.extend(displaced);

    let id = match existing {
        Some(e) if e.deleted_at.is_none() && e.same_as(r) => {
            out.unmodified.push(e.id);
            e.id
        }
        Some(e) => {
            sqlx::query(
                "UPDATE repo SET name = $2, uri = $3, url = $4, description = $5, fork = $6,
                     archived = $7, private = $8, visibility = $9, stars = $10, topics = $11,
                     default_branch = $12, last_activity_at = $13, metadata = $14,
                     deleted_at = NULL, updated_at = now()
                 WHERE id = $1",
            )
            .bind(e.id)
            .bind(&r.name)
            .bind(&r.uri)
            .bind(&r.url)
            .bind(&r.description)
            .bind(r.fork)
            .bind(r.archived)
            .bind(r.private)
            .bind(r.visibility)
            .bind(r.stars)
            .bind(&r.topics)
            .bind(&r.default_branch)
            .bind(r.last_activity_at)
            .bind(&r.metadata)
            .execute(&mut *tx)
            .await?;
            let revived = e.deleted_at.is_some();
            if revived {
                out.added.push(e.id);
                ensure_gitserver_row(tx, e.id).await?;
                enqueue_update(
                    tx,
                    e.id,
                    RepoUpdatePriority::NORMAL,
                    RepoUpdateReason::Discovered,
                )
                .await?;
                out.fetches_queued.push(e.id);
            } else {
                out.modified.push(e.id);
                let advanced = match (e.last_activity_at, r.last_activity_at) {
                    (Some(old), Some(new)) => new > old,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if advanced {
                    enqueue_update(
                        tx,
                        e.id,
                        RepoUpdatePriority::HIGH,
                        RepoUpdateReason::Activity,
                    )
                    .await?;
                    out.fetches_queued.push(e.id);
                }
            }
            e.id
        }
        None => {
            let id: RepoId = sqlx::query_scalar(
                "INSERT INTO repo (name, uri, url, description, fork, archived, private,
                     visibility, stars, topics, default_branch, last_activity_at, external_id,
                     external_service_type, external_service_id, metadata)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
                 RETURNING id",
            )
            .bind(&r.name)
            .bind(&r.uri)
            .bind(&r.url)
            .bind(&r.description)
            .bind(r.fork)
            .bind(r.archived)
            .bind(r.private)
            .bind(r.visibility)
            .bind(r.stars)
            .bind(&r.topics)
            .bind(&r.default_branch)
            .bind(r.last_activity_at)
            .bind(&r.external_id)
            .bind(&r.external_service_type)
            .bind(&r.external_service_id)
            .bind(&r.metadata)
            .fetch_one(&mut *tx)
            .await?;
            ensure_gitserver_row(tx, id).await?;
            enqueue_update(
                tx,
                id,
                RepoUpdatePriority::NORMAL,
                RepoUpdateReason::Discovered,
            )
            .await?;
            out.added.push(id);
            out.fetches_queued.push(id);
            id
        }
    };

    sqlx::query(
        "INSERT INTO external_service_repos (external_service_id, repo_id, clone_url)
         VALUES ($1, $2, $3)
         ON CONFLICT (external_service_id, repo_id) DO UPDATE SET clone_url = EXCLUDED.clone_url
         WHERE external_service_repos.clone_url <> EXCLUDED.clone_url",
    )
    .bind(service)
    .bind(id)
    .bind(&r.clone_url)
    .execute(&mut *tx)
    .await?;
    out.ids.push(id);
    Ok(())
}

async fn ensure_gitserver_row(tx: &mut PgConnection, id: RepoId) -> Result<()> {
    sqlx::query("INSERT INTO gitserver_repos (repo_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Unlinks every repo of `service` not in `keep` and soft-deletes the unlinked repos no other
/// live connection lists. Returns the number of repos soft-deleted.
pub(crate) async fn unlink_repos(
    tx: &mut PgConnection,
    service: ExternalServiceId,
    keep: &[RepoId],
) -> Result<u64> {
    let keep: HashSet<RepoId> = keep.iter().copied().collect();
    let keep: Vec<RepoId> = keep.into_iter().collect();
    let unlinked: Vec<RepoId> = sqlx::query_scalar(
        "DELETE FROM external_service_repos
         WHERE external_service_id = $1 AND NOT (repo_id = ANY($2))
         RETURNING repo_id",
    )
    .bind(service)
    .bind(&keep)
    .fetch_all(&mut *tx)
    .await?;
    if unlinked.is_empty() {
        return Ok(0);
    }
    let r = sqlx::query(
        "UPDATE repo SET deleted_at = now(), updated_at = now()
         WHERE id = ANY($1) AND deleted_at IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM external_service_repos esr
               JOIN external_services es ON es.id = esr.external_service_id
               WHERE esr.repo_id = repo.id AND es.deleted_at IS NULL)",
    )
    .bind(&unlinked)
    .execute(&mut *tx)
    .await?;
    // Pending clone/fetch requests for deleted repos are moot.
    sqlx::query(
        "UPDATE repo_update_jobs SET state = 'canceled', finished_at = now(), cancel = true
         WHERE state = 'queued' AND repo_id = ANY($1)
           AND repo_id IN (SELECT id FROM repo WHERE deleted_at IS NOT NULL)",
    )
    .bind(&unlinked)
    .execute(&mut *tx)
    .await?;
    Ok(r.rows_affected())
}
