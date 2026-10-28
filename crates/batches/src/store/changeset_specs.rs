//! `changeset_specs`: what one changeset should look like.
//!
//! Specs come from server-side executions (attached to their bulk change spec right away) or are
//! uploaded by a local run (`createChangesetSpec(s)`) and attached when the run creates its
//! bulk change spec. Unattached specs expire.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, DbError, Result};
use sourcegull_types::{RepoId, UserId};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Row};

use super::str_enum;

/// How long an uploaded changeset spec waits to be attached to a bulk change spec (2 days).
pub const CHANGESET_SPEC_TTL: Duration = Duration::from_secs(2 * 24 * 3600);

str_enum!(
    /// What a changeset spec describes.
    ChangesetSpecType {
        /// A new changeset: a diff committed to a branch.
        Branch => "branch",
        /// An existing changeset to import, by external id.
        Existing => "existing",
    }
);

/// The `published` field of a changeset spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// `true` or `false`.
    Bool(bool),
    /// `"draft"`.
    Draft,
}

impl Published {
    fn to_json(self) -> serde_json::Value {
        match self {
            Self::Bool(b) => serde_json::Value::Bool(b),
            Self::Draft => serde_json::Value::String("draft".into()),
        }
    }

    fn from_json(v: &serde_json::Value) -> Option<Self> {
        match v {
            serde_json::Value::Bool(b) => Some(Self::Bool(*b)),
            serde_json::Value::String(s) if s == "draft" => Some(Self::Draft),
            _ => None,
        }
    }
}

/// A changeset spec to create. For [`ChangesetSpecType::Branch`], `head_ref` and `base_ref`
/// are required; for [`ChangesetSpecType::Existing`], `external_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChangesetSpec {
    pub batch_spec_id: Option<i64>,
    pub repo_id: RepoId,
    pub user_id: Option<UserId>,
    pub kind: ChangesetSpecType,
    pub external_id: Option<String>,
    pub base_ref: Option<String>,
    pub base_rev: Option<String>,
    pub head_ref: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// `None`: decided in the UI (`publicationStates` on apply).
    pub published: Option<Published>,
    pub diff: Option<Vec<u8>>,
    pub diff_stat_added: Option<i32>,
    pub diff_stat_deleted: Option<i32>,
    pub commit_message: Option<String>,
    pub commit_author_name: Option<String>,
    pub commit_author_email: Option<String>,
    /// Push to a fork in this namespace (`changesetTemplate.fork`).
    pub fork_namespace: Option<String>,
}

impl NewChangesetSpec {
    /// A spec of `kind` in `repo` with every optional field empty.
    pub fn new(repo_id: RepoId, kind: ChangesetSpecType) -> Self {
        Self {
            batch_spec_id: None,
            repo_id,
            user_id: None,
            kind,
            external_id: None,
            base_ref: None,
            base_rev: None,
            head_ref: None,
            title: None,
            body: None,
            published: None,
            diff: None,
            diff_stat_added: None,
            diff_stat_deleted: None,
            commit_message: None,
            commit_author_name: None,
            commit_author_email: None,
            fork_namespace: None,
        }
    }
}

/// A stored changeset spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetSpec {
    pub id: i64,
    /// The id used in GraphQL ids.
    pub rand_id: String,
    pub batch_spec_id: Option<i64>,
    pub repo_id: RepoId,
    pub user_id: Option<UserId>,
    pub kind: ChangesetSpecType,
    pub external_id: Option<String>,
    pub base_ref: Option<String>,
    pub base_rev: Option<String>,
    pub head_ref: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub published: Option<Published>,
    pub diff: Option<Vec<u8>>,
    pub diff_stat_added: Option<i32>,
    pub diff_stat_deleted: Option<i32>,
    pub commit_message: Option<String>,
    pub commit_author_name: Option<String>,
    pub commit_author_email: Option<String>,
    pub fork_namespace: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ChangesetSpec {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        let kind: String = row.try_get("type")?;
        let published: Option<Json<serde_json::Value>> = row.try_get("published")?;
        Ok(Self {
            id: row.try_get("id")?,
            rand_id: row.try_get("rand_id")?,
            batch_spec_id: row.try_get("batch_spec_id")?,
            repo_id: RepoId(row.try_get("repo_id")?),
            user_id: row.try_get("user_id")?,
            kind: ChangesetSpecType::parse(&kind).ok_or_else(|| sqlx::Error::ColumnDecode {
                index: "type".into(),
                source: format!("unknown changeset spec type {kind:?}").into(),
            })?,
            external_id: row.try_get("external_id")?,
            base_ref: row.try_get("base_ref")?,
            base_rev: row.try_get("base_rev")?,
            head_ref: row.try_get("head_ref")?,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            published: published.and_then(|Json(v)| Published::from_json(&v)),
            diff: row.try_get("diff")?,
            diff_stat_added: row.try_get("diff_stat_added")?,
            diff_stat_deleted: row.try_get("diff_stat_deleted")?,
            commit_message: row.try_get("commit_message")?,
            commit_author_name: row.try_get("commit_author_name")?,
            commit_author_email: row.try_get("commit_author_email")?,
            fork_namespace: row.try_get("fork_namespace")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Filters of [`ChangesetSpecStore::list`]. Specs are listed in id order.
#[derive(Debug, Clone, Default)]
pub struct ListChangesetSpecs {
    pub batch_spec_id: Option<i64>,
    pub ids: Option<Vec<i64>>,
    /// Only specs with an id above this cursor.
    pub after_id: Option<i64>,
    /// Page size; no limit when 0.
    pub limit: i64,
}

const INSERT: &str = "INSERT INTO changeset_specs (batch_spec_id, repo_id, user_id, type,
         external_id, base_ref, base_rev, head_ref, title, body, published, diff,
         diff_stat_added, diff_stat_deleted, commit_message, commit_author_name,
         commit_author_email, fork_namespace)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
     RETURNING *";

/// `WHERE` clause of [`ListChangesetSpecs`] over `changeset_specs s`, binds `$1`..`$3`.
/// Specs in deleted repositories are hidden.
const LIST_FILTER: &str = "($1::bigint IS NULL OR s.batch_spec_id = $1)
    AND ($2::bigint[] IS NULL OR s.id = ANY($2))
    AND ($3::bigint IS NULL OR s.id > $3)
    AND EXISTS (SELECT 1 FROM repo r WHERE r.id = s.repo_id AND r.deleted_at IS NULL)";

/// Store for changeset specs.
#[derive(Debug, Clone)]
pub struct ChangesetSpecStore {
    pool: PgPool,
}

impl ChangesetSpecStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Creates specs in one transaction, in input order.
    pub async fn create_many(&self, specs: &[NewChangesetSpec]) -> Result<Vec<ChangesetSpec>> {
        let mut tx = self.pool.begin().await?;
        let out = insert_specs(&mut tx, specs).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Creates one spec.
    pub async fn create(&self, spec: &NewChangesetSpec) -> Result<ChangesetSpec> {
        let mut specs = self.create_many(std::slice::from_ref(spec)).await?;
        specs.pop().ok_or(DbError::NotFound("changeset spec"))
    }

    /// Looks up a spec.
    pub async fn get(&self, id: i64) -> Result<Option<ChangesetSpec>> {
        Ok(
            sqlx::query_as("SELECT * FROM changeset_specs WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Looks up a spec by its GraphQL id.
    pub async fn get_by_rand_id(&self, rand_id: &str) -> Result<Option<ChangesetSpec>> {
        Ok(
            sqlx::query_as("SELECT * FROM changeset_specs WHERE rand_id = $1")
                .bind(rand_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Looks up specs by GraphQL id (missing ones are left out), in id order.
    pub async fn get_by_rand_ids(&self, rand_ids: &[String]) -> Result<Vec<ChangesetSpec>> {
        Ok(
            sqlx::query_as("SELECT * FROM changeset_specs WHERE rand_id = ANY($1) ORDER BY id")
                .bind(rand_ids)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Specs matching `opts`, in id order.
    pub async fn list(&self, opts: &ListChangesetSpecs) -> Result<Vec<ChangesetSpec>> {
        Ok(sqlx::query_as(&format!(
            "SELECT s.* FROM changeset_specs s WHERE {LIST_FILTER} ORDER BY s.id
             LIMIT CASE WHEN $4::bigint > 0 THEN $4::bigint END"
        ))
        .bind(opts.batch_spec_id)
        .bind(&opts.ids)
        .bind(opts.after_id)
        .bind(opts.limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The number of specs matching `opts` (ignoring `after_id` and `limit`).
    pub async fn count(&self, opts: &ListChangesetSpecs) -> Result<i64> {
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM changeset_specs s WHERE {LIST_FILTER}"
        ))
        .bind(opts.batch_spec_id)
        .bind(&opts.ids)
        .bind(None::<i64>)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The summed diff stats (added, deleted) of a bulk change spec's changeset specs.
    pub async fn diff_stat(&self, batch_spec_id: i64) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT COALESCE(sum(diff_stat_added), 0)::bigint, COALESCE(sum(diff_stat_deleted), 0)::bigint
             FROM changeset_specs WHERE batch_spec_id = $1",
        )
        .bind(batch_spec_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Attaches unattached specs (uploaded by a local run) to a bulk change spec. Returns how many
    /// were attached; specs already attached elsewhere are left alone.
    pub async fn attach_to_batch_spec(&self, batch_spec_id: i64, ids: &[i64]) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE changeset_specs SET batch_spec_id = $1, updated_at = now()
             WHERE id = ANY($2) AND batch_spec_id IS NULL",
        )
        .bind(batch_spec_id)
        .bind(ids)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Deletes a spec no changeset uses.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM changeset_specs WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Deletes a bulk change spec's specs that no changeset uses (when its input is replaced).
    /// Returns how many were deleted.
    pub async fn delete_for_batch_spec(&self, batch_spec_id: i64) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM changeset_specs s WHERE s.batch_spec_id = $1
               AND NOT EXISTS (SELECT 1 FROM changesets c
                               WHERE c.current_spec_id = s.id OR c.previous_spec_id = s.id)",
        )
        .bind(batch_spec_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Deletes specs never attached to a bulk change spec within `ttl` and not used by a changeset.
    pub async fn delete_unattached_older_than(&self, ttl: Duration) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM changeset_specs s
             WHERE s.batch_spec_id IS NULL
               AND s.created_at < now() - make_interval(secs => $1::float8)
               AND NOT EXISTS (SELECT 1 FROM changesets c
                               WHERE c.current_spec_id = s.id OR c.previous_spec_id = s.id)",
        )
        .bind(ttl.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

fn valid(s: &NewChangesetSpec) -> bool {
    match s.kind {
        ChangesetSpecType::Branch => s.head_ref.is_some() && s.base_ref.is_some(),
        ChangesetSpecType::Existing => s.external_id.is_some(),
    }
}

/// Inserts specs on `conn` (inside the caller's transaction), in input order.
pub(super) async fn insert_specs(
    conn: &mut PgConnection,
    specs: &[NewChangesetSpec],
) -> Result<Vec<ChangesetSpec>> {
    let mut out = Vec::with_capacity(specs.len());
    for s in specs {
        if !valid(s) {
            return Err(DbError::Invalid(
                "changeset spec is missing required fields",
            ));
        }
        out.push(
            sqlx::query_as(INSERT)
                .bind(s.batch_spec_id)
                .bind(s.repo_id.get())
                .bind(s.user_id)
                .bind(s.kind.as_str())
                .bind(&s.external_id)
                .bind(&s.base_ref)
                .bind(&s.base_rev)
                .bind(&s.head_ref)
                .bind(&s.title)
                .bind(&s.body)
                .bind(s.published.map(|p| Json(p.to_json())))
                .bind(&s.diff)
                .bind(s.diff_stat_added)
                .bind(s.diff_stat_deleted)
                .bind(&s.commit_message)
                .bind(&s.commit_author_name)
                .bind(&s.commit_author_email)
                .bind(&s.fork_namespace)
                .fetch_one(&mut *conn)
                .await?,
        );
    }
    Ok(out)
}
