//! `codeintel_configuration_policies` store: which git objects keep precise indexes and, with
//! auto-indexing, which commits are indexed.
//!
//! Policies are few (tens to hundreds), so listing reads them all and applies repository
//! patterns in Rust with [`sourcegull_types::glob`] instead of keeping a lookup table of matching
//! repositories.

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, Result};
use sourcegull_types::RepoId;
use sourcegull_types::glob::Glob;
use sqlx::PgPool;

/// The kind of git object a policy names (GraphQL's `GitObjectType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitObjectType {
    /// One commit, named by its oid (or any revision).
    Commit,
    /// Branches (`GIT_TREE` in the API).
    Branch,
    Tag,
}

impl GitObjectType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "GIT_COMMIT",
            Self::Branch => "GIT_TREE",
            Self::Tag => "GIT_TAG",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "GIT_COMMIT" => Self::Commit,
            "GIT_TAG" => Self::Tag,
            _ => Self::Branch,
        }
    }
}

/// A configuration policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationPolicy {
    pub id: i32,
    /// The one repository the policy applies to.
    pub repo_id: Option<RepoId>,
    /// Globs on repository names the policy applies to.
    pub repository_patterns: Option<Vec<String>>,
    pub name: String,
    pub object_type: GitObjectType,
    pub pattern: String,
    pub protected: bool,
    pub retention_enabled: bool,
    pub retention_duration_hours: Option<i32>,
    pub retain_intermediate_commits: bool,
    pub indexing_enabled: bool,
    pub index_commit_max_age_hours: Option<i32>,
    pub index_intermediate_commits: bool,
    pub syntactic_indexing_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ConfigurationPolicy {
    /// Whether the policy applies to the repository `name` with id `id`.
    pub fn applies_to(&self, id: RepoId, name: &str) -> bool {
        if let Some(repo) = self.repo_id {
            return repo == id;
        }
        match &self.repository_patterns {
            None => true,
            Some(patterns) => patterns
                .iter()
                .any(|p| Glob::new(p).is_ok_and(|g| g.is_match(name))),
        }
    }

    /// Whether the policy applies to every repository.
    pub fn is_global(&self) -> bool {
        self.repo_id.is_none() && self.repository_patterns.is_none()
    }
}

const COLUMNS: &str = "id, repository_id, repository_patterns, name, type, pattern, protected, \
    retention_enabled, retention_duration_hours, retain_intermediate_commits, indexing_enabled, \
    index_commit_max_age_hours, index_intermediate_commits, syntactic_indexing_enabled, \
    created_at, updated_at";

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for ConfigurationPolicy {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let object_type: String = row.try_get("type")?;
        Ok(ConfigurationPolicy {
            id: row.try_get("id")?,
            repo_id: row.try_get::<Option<i32>, _>("repository_id")?.map(RepoId),
            repository_patterns: row.try_get("repository_patterns")?,
            name: row.try_get("name")?,
            object_type: GitObjectType::parse(&object_type),
            pattern: row.try_get("pattern")?,
            protected: row.try_get("protected")?,
            retention_enabled: row.try_get("retention_enabled")?,
            retention_duration_hours: row.try_get("retention_duration_hours")?,
            retain_intermediate_commits: row.try_get("retain_intermediate_commits")?,
            indexing_enabled: row.try_get("indexing_enabled")?,
            index_commit_max_age_hours: row.try_get("index_commit_max_age_hours")?,
            index_intermediate_commits: row.try_get("index_intermediate_commits")?,
            syntactic_indexing_enabled: row.try_get("syntactic_indexing_enabled")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// The attributes of a new or updated policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyFields {
    pub repository_patterns: Option<Vec<String>>,
    pub name: String,
    pub object_type: GitObjectType,
    pub pattern: String,
    pub retention_enabled: bool,
    pub retention_duration_hours: Option<i32>,
    pub retain_intermediate_commits: bool,
    pub indexing_enabled: bool,
    pub index_commit_max_age_hours: Option<i32>,
    pub index_intermediate_commits: bool,
    pub syntactic_indexing_enabled: bool,
}

/// Filters of [`PolicyStore::list`]. `None` flags don't filter.
#[derive(Debug, Clone, Default)]
pub struct ListPolicies {
    /// Only policies that apply to this repository (id and name): global ones, its own,
    /// and those whose repository patterns match its name.
    pub repo: Option<(RepoId, String)>,
    /// Case-insensitive substring of the name.
    pub query: Option<String>,
    pub for_retention: Option<bool>,
    pub for_indexing: Option<bool>,
    pub for_syntactic_indexing: Option<bool>,
    pub protected: Option<bool>,
}

/// Why a policy write was refused.
#[derive(Debug, thiserror::Error)]
pub enum PolicyWriteError {
    #[error("configuration policy not found")]
    NotFound,
    #[error("protected configuration policies cannot be deleted")]
    DeleteProtected,
    #[error(
        "protected configuration policies must keep the same name, type, pattern and \
         intermediate commit settings"
    )]
    ChangeProtected,
    #[error(transparent)]
    Db(#[from] sourcegull_db::DbError),
}

impl From<sqlx::Error> for PolicyWriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Store for `codeintel_configuration_policies`.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    pool: PgPool,
}

impl PolicyStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    pub(crate) fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Policies matching `opts`: protected ones first, then by id.
    pub async fn list(&self, opts: &ListPolicies) -> Result<Vec<ConfigurationPolicy>> {
        let pattern = opts
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| {
                format!(
                    "%{}%",
                    q.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });
        let rows: Vec<ConfigurationPolicy> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_configuration_policies
             WHERE ($1::int IS NULL OR repository_id IS NULL OR repository_id = $1)
               AND ($2::text IS NULL OR name ILIKE $2)
               AND ($3::bool IS NULL OR retention_enabled = $3)
               AND ($4::bool IS NULL OR indexing_enabled = $4)
               AND ($5::bool IS NULL OR syntactic_indexing_enabled = $5)
               AND ($6::bool IS NULL OR protected = $6)
             ORDER BY protected DESC, id"
        ))
        .bind(opts.repo.as_ref().map(|(id, _)| id.get()))
        .bind(pattern)
        .bind(opts.for_retention)
        .bind(opts.for_indexing)
        .bind(opts.for_syntactic_indexing)
        .bind(opts.protected)
        .fetch_all(&self.pool)
        .await?;
        Ok(match &opts.repo {
            None => rows,
            Some((id, name)) => rows
                .into_iter()
                .filter(|p| p.applies_to(*id, name))
                .collect(),
        })
    }

    pub async fn get(&self, id: i32) -> Result<Option<ConfigurationPolicy>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_configuration_policies WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Creates a policy, for `repo` only or (without one) global or by repository patterns.
    pub async fn create(
        &self,
        repo: Option<RepoId>,
        f: &PolicyFields,
    ) -> Result<ConfigurationPolicy> {
        Ok(sqlx::query_as(&format!(
            "INSERT INTO codeintel_configuration_policies (repository_id, repository_patterns,
                name, type, pattern, retention_enabled, retention_duration_hours,
                retain_intermediate_commits, indexing_enabled, index_commit_max_age_hours,
                index_intermediate_commits, syntactic_indexing_enabled)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             RETURNING {COLUMNS}"
        ))
        .bind(repo.map(|r| r.get()))
        .bind(if repo.is_some() {
            None
        } else {
            f.repository_patterns.clone()
        })
        .bind(&f.name)
        .bind(f.object_type.as_str())
        .bind(&f.pattern)
        .bind(f.retention_enabled)
        .bind(f.retention_duration_hours)
        .bind(f.retain_intermediate_commits)
        .bind(f.indexing_enabled)
        .bind(f.index_commit_max_age_hours)
        .bind(f.index_intermediate_commits)
        .bind(f.syntactic_indexing_enabled)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Updates a policy. A protected policy keeps its name, type, pattern, repository
    /// patterns and intermediate commit settings; its durations and toggles may change.
    /// Repository patterns of a policy for one repository are ignored.
    pub async fn update(
        &self,
        id: i32,
        f: &PolicyFields,
    ) -> std::result::Result<ConfigurationPolicy, PolicyWriteError> {
        let mut tx = self.pool.begin().await?;
        let current: ConfigurationPolicy = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM codeintel_configuration_policies WHERE id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PolicyWriteError::NotFound)?;
        let patterns = if current.repo_id.is_some() {
            None
        } else {
            f.repository_patterns.clone()
        };
        if current.protected
            && (current.name != f.name
                || current.object_type != f.object_type
                || current.pattern != f.pattern
                || current.repository_patterns != patterns
                || current.retain_intermediate_commits != f.retain_intermediate_commits
                || current.index_intermediate_commits != f.index_intermediate_commits)
        {
            return Err(PolicyWriteError::ChangeProtected);
        }
        let updated = sqlx::query_as(&format!(
            "UPDATE codeintel_configuration_policies SET repository_patterns = $2, name = $3,
                type = $4, pattern = $5, retention_enabled = $6, retention_duration_hours = $7,
                retain_intermediate_commits = $8, indexing_enabled = $9,
                index_commit_max_age_hours = $10, index_intermediate_commits = $11,
                syntactic_indexing_enabled = $12, updated_at = now()
             WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(patterns)
        .bind(&f.name)
        .bind(f.object_type.as_str())
        .bind(&f.pattern)
        .bind(f.retention_enabled)
        .bind(f.retention_duration_hours)
        .bind(f.retain_intermediate_commits)
        .bind(f.indexing_enabled)
        .bind(f.index_commit_max_age_hours)
        .bind(f.index_intermediate_commits)
        .bind(f.syntactic_indexing_enabled)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Deletes an unprotected policy.
    pub async fn delete(&self, id: i32) -> std::result::Result<(), PolicyWriteError> {
        let protected: Option<bool> = sqlx::query_scalar(
            "DELETE FROM codeintel_configuration_policies WHERE id = $1 AND NOT protected
             RETURNING protected",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        if protected.is_some() {
            return Ok(());
        }
        match self.get(id).await? {
            Some(_) => Err(PolicyWriteError::DeleteProtected),
            None => Err(PolicyWriteError::NotFound),
        }
    }
}
