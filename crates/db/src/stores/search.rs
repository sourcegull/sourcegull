//! Search contexts and saved searches.
//!
//! A context belongs to a user, an org or the instance ([`Namespace`]) and
//! either lists repositories with revisions or is defined by a query of
//! repo filters. Saved searches belong to a user or an org. Access rules
//! (who may read or change which) are the API's; these stores take the
//! viewer's affiliations as filters.

use chrono::{DateTime, Utc};
use sourcegull_types::{OrgId, RepoId, UserId};
use sqlx::PgPool;

use crate::{DbError, Result};

/// The owner of a context or saved search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Namespace {
    User(UserId),
    Org(OrgId),
}

impl Namespace {
    /// The `namespace_user_id` and `namespace_org_id` column values of `owner`.
    pub fn columns(owner: Option<Namespace>) -> (Option<UserId>, Option<OrgId>) {
        match owner {
            Some(Namespace::User(u)) => (Some(u), None),
            Some(Namespace::Org(o)) => (None, Some(o)),
            None => (None, None),
        }
    }

    /// The namespace those two columns hold; `None` when both are null.
    pub fn from_columns(user: Option<UserId>, org: Option<OrgId>) -> Option<Namespace> {
        match (user, org) {
            (Some(u), _) => Some(Namespace::User(u)),
            (None, Some(o)) => Some(Namespace::Org(o)),
            (None, None) => None,
        }
    }
}

/// A search context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchContext {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub public: bool,
    /// `None`: an instance-level context.
    pub namespace: Option<Namespace>,
    /// The owning user's or org's name.
    pub namespace_name: Option<String>,
    /// A query-defined context's repo filters.
    pub query: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The reading user has it as their default.
    pub viewer_default: bool,
    /// The reading user starred it.
    pub viewer_starred: bool,
}

impl SearchContext {
    /// The spec used in `context:`: `name` or `@owner/name`.
    pub fn spec(&self) -> String {
        match &self.namespace_name {
            Some(owner) => format!("@{owner}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct ContextRow {
    id: i64,
    name: String,
    description: String,
    public: bool,
    namespace_user_id: Option<UserId>,
    namespace_org_id: Option<OrgId>,
    namespace_name: Option<String>,
    query: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    viewer_default: bool,
    viewer_starred: bool,
}

impl From<ContextRow> for SearchContext {
    fn from(r: ContextRow) -> Self {
        SearchContext {
            id: r.id,
            name: r.name,
            description: r.description,
            public: r.public,
            namespace: Namespace::from_columns(r.namespace_user_id, r.namespace_org_id),
            namespace_name: r.namespace_name,
            query: r.query,
            created_at: r.created_at,
            updated_at: r.updated_at,
            viewer_default: r.viewer_default,
            viewer_starred: r.viewer_starred,
        }
    }
}

/// Who reads search contexts. Private contexts are visible to their owning
/// user, the members of their owning org, and (instance-level ones) site
/// admins; nobody else, site admins included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAccess {
    /// A user, or anonymous (`None`).
    Viewer(Option<UserId>),
    /// Internal callers: every context.
    All,
}

/// Starts a context read for `access`: the columns, with the viewer bound
/// first (`$1`), and the access condition. Callers append `AND ...`.
fn select_contexts(
    columns: &str,
    access: ContextAccess,
) -> sqlx::QueryBuilder<'static, sqlx::Postgres> {
    let viewer = match access {
        ContextAccess::Viewer(u) => u,
        ContextAccess::All => None,
    };
    let mut b = sqlx::QueryBuilder::new("WITH viewer AS (SELECT ");
    b.push_bind(viewer).push(
        "::bigint AS id)
        SELECT ",
    );
    b.push(columns).push(
        " FROM search_contexts c
        CROSS JOIN viewer v
        LEFT JOIN users u ON u.id = c.namespace_user_id
        LEFT JOIN orgs o ON o.id = c.namespace_org_id
        LEFT JOIN search_context_stars s ON s.search_context_id = c.id AND s.user_id = v.id
        LEFT JOIN search_context_default d ON d.search_context_id = c.id AND d.user_id = v.id
        WHERE u.deleted_at IS NULL AND o.deleted_at IS NULL",
    );
    if access != ContextAccess::All {
        b.push(
            " AND (c.public
            OR c.namespace_user_id = v.id
            OR (c.namespace_org_id IS NOT NULL AND EXISTS (SELECT 1 FROM org_members m
                WHERE m.org_id = c.namespace_org_id AND m.user_id = v.id))
            OR (c.namespace_user_id IS NULL AND c.namespace_org_id IS NULL
                AND EXISTS (SELECT 1 FROM users a WHERE a.id = v.id AND a.site_admin
                    AND a.deleted_at IS NULL)))",
        );
    }
    b
}

const CONTEXT_COLUMNS: &str = "c.id, c.name, c.description, c.public, c.namespace_user_id,
    c.namespace_org_id, coalesce(u.username, o.name) AS namespace_name, c.query,
    c.created_at, c.updated_at, d.user_id IS NOT NULL AS viewer_default,
    s.user_id IS NOT NULL AS viewer_starred";

/// A repository in a context, with the revisions searched ("" is the
/// default branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchContextRepo {
    pub repo_id: RepoId,
    pub repo_name: String,
    pub revisions: Vec<String>,
}

/// What a context is made of.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchContextInput {
    pub name: String,
    pub description: String,
    pub public: bool,
    pub query: Option<String>,
    /// Repositories and revisions; ignored when `query` is set.
    pub repos: Vec<(RepoId, Vec<String>)>,
}

/// How contexts are ordered after the viewer's default and starred ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextOrder {
    /// Owner name (instance-level first), then name.
    #[default]
    Spec,
    UpdatedAt,
}

/// Which contexts to list.
#[derive(Debug, Clone)]
pub struct ListContextsOptions {
    pub access: ContextAccess,
    /// Case-insensitive substring of the spec.
    pub query: Option<String>,
    /// Only contexts of these namespaces (`None` in the list: instance).
    pub namespaces: Option<Vec<Option<Namespace>>>,
    pub order: ContextOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListContextsOptions {
    fn default() -> Self {
        Self {
            access: ContextAccess::Viewer(None),
            query: None,
            namespaces: None,
            order: ContextOrder::Spec,
            descending: false,
            limit: 50,
            offset: 0,
        }
    }
}

/// Reads and writes search contexts.
#[derive(Debug, Clone)]
pub struct SearchContextStore {
    pool: PgPool,
}

impl SearchContextStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates a context. Fails with [`DbError::AlreadyExists`] when the
    /// owner already has one with that name.
    pub async fn create(
        &self,
        namespace: Option<Namespace>,
        input: &SearchContextInput,
    ) -> Result<SearchContext> {
        let (user, org) = Namespace::columns(namespace);
        let mut tx = self.pool.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO search_contexts
                (name, description, public, namespace_user_id, namespace_org_id, query)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(&input.name)
        .bind(&input.description)
        .bind(input.public)
        .bind(user)
        .bind(org)
        .bind(&input.query)
        .fetch_one(&mut *tx)
        .await?;
        set_repos(&mut tx, id, input).await?;
        tx.commit().await?;
        self.get(id, ContextAccess::All)
            .await?
            .ok_or(DbError::NotFound("search context"))
    }

    /// Replaces a context's name, description, visibility and contents.
    pub async fn update(&self, id: i64, input: &SearchContextInput) -> Result<SearchContext> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query(
            "UPDATE search_contexts SET name = $2, description = $3, public = $4, query = $5,
                updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(&input.name)
        .bind(&input.description)
        .bind(input.public)
        .bind(&input.query)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("search context"));
        }
        sqlx::query("DELETE FROM search_context_repos WHERE search_context_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        set_repos(&mut tx, id, input).await?;
        tx.commit().await?;
        self.get(id, ContextAccess::All)
            .await?
            .ok_or(DbError::NotFound("search context"))
    }

    pub async fn delete(&self, id: i64) -> Result<()> {
        let r = sqlx::query("DELETE FROM search_contexts WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("search context"));
        }
        Ok(())
    }

    /// The context with this id, if `access` may read it.
    pub async fn get(&self, id: i64, access: ContextAccess) -> Result<Option<SearchContext>> {
        let mut b = select_contexts(CONTEXT_COLUMNS, access);
        b.push(" AND c.id = ").push_bind(id);
        let row: Option<ContextRow> = b.build_query_as().fetch_optional(&self.pool).await?;
        Ok(row.map(Into::into))
    }

    /// The context with this spec (`name` or `@owner/name`; names are
    /// case-insensitive), if `access` may read it.
    pub async fn get_by_spec(
        &self,
        spec: &str,
        access: ContextAccess,
    ) -> Result<Option<SearchContext>> {
        let (owner, name) = match spec.strip_prefix('@') {
            Some(rest) => match rest.split_once('/') {
                Some((owner, name)) => (Some(owner), name),
                None => return Ok(None),
            },
            None => (None, spec),
        };
        let mut b = select_contexts(CONTEXT_COLUMNS, access);
        b.push(" AND lower(c.name) = lower(")
            .push_bind(name.to_owned())
            .push(")");
        match owner {
            None => b.push(" AND c.namespace_user_id IS NULL AND c.namespace_org_id IS NULL"),
            Some(owner) => b
                .push(" AND lower(coalesce(u.username, o.name)) = lower(")
                .push_bind(owner.to_owned())
                .push(")"),
        };
        let row: Option<ContextRow> = b.build_query_as().fetch_optional(&self.pool).await?;
        Ok(row.map(Into::into))
    }

    /// Contexts `opts.access` may read, with the total count: the viewer's
    /// default first, then starred ones, then by `opts.order`.
    pub async fn list(&self, opts: &ListContextsOptions) -> Result<(Vec<SearchContext>, i64)> {
        let mut b = select_contexts(CONTEXT_COLUMNS, opts.access);
        filters(&mut b, opts);
        let dir = if opts.descending { "DESC" } else { "ASC" };
        b.push(" ORDER BY viewer_default DESC, viewer_starred DESC, ");
        b.push(match opts.order {
            ContextOrder::Spec => {
                format!("coalesce(u.username, o.name, '') {dir}, lower(c.name) {dir}, c.id")
            }
            ContextOrder::UpdatedAt => format!("c.updated_at {dir}, c.id"),
        });
        b.push(" LIMIT ")
            .push_bind(opts.limit)
            .push(" OFFSET ")
            .push_bind(opts.offset);
        let rows: Vec<ContextRow> = b.build_query_as().fetch_all(&self.pool).await?;
        let mut count = select_contexts("count(*)", opts.access);
        filters(&mut count, opts);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;
        Ok((rows.into_iter().map(Into::into).collect(), total))
    }

    /// The repositories of a listed context.
    pub async fn repos(&self, id: i64) -> Result<Vec<SearchContextRepo>> {
        let rows: Vec<(RepoId, String, Vec<String>)> = sqlx::query_as(
            "SELECT r.id, r.name, array_agg(s.revision ORDER BY s.revision)
             FROM search_context_repos s JOIN repo r ON r.id = s.repo_id
             WHERE s.search_context_id = $1 AND r.deleted_at IS NULL
             GROUP BY r.id, r.name ORDER BY r.name",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(repo_id, repo_name, revisions)| SearchContextRepo {
                repo_id,
                repo_name,
                revisions,
            })
            .collect())
    }

    /// Stars or unstars a context for a user.
    pub async fn set_starred(&self, id: i64, user: UserId, starred: bool) -> Result<()> {
        let sql = if starred {
            "INSERT INTO search_context_stars (search_context_id, user_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING"
        } else {
            "DELETE FROM search_context_stars WHERE search_context_id = $1 AND user_id = $2"
        };
        sqlx::query(sql)
            .bind(id)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Sets a user's default context (`None`: global).
    pub async fn set_default(&self, user: UserId, id: Option<i64>) -> Result<()> {
        match id {
            Some(id) => {
                sqlx::query(
                    "INSERT INTO search_context_default (user_id, search_context_id)
                     VALUES ($1, $2) ON CONFLICT (user_id)
                     DO UPDATE SET search_context_id = excluded.search_context_id",
                )
                .bind(user)
                .bind(id)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query("DELETE FROM search_context_default WHERE user_id = $1")
                    .bind(user)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(())
    }

    /// A user's default context, if they set one and may still read it;
    /// `None` is global.
    pub async fn default_for(&self, user: UserId) -> Result<Option<SearchContext>> {
        let mut b = select_contexts(CONTEXT_COLUMNS, ContextAccess::Viewer(Some(user)));
        b.push(" AND d.user_id IS NOT NULL");
        let row: Option<ContextRow> = b.build_query_as().fetch_optional(&self.pool).await?;
        Ok(row.map(Into::into))
    }
}

/// Adds the list filters to a context query.
fn filters(b: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>, opts: &ListContextsOptions) {
    if let Some(q) = &opts.query {
        b.push(
            " AND (CASE WHEN coalesce(u.username, o.name) IS NULL THEN c.name
                 ELSE '@' || coalesce(u.username, o.name) || '/' || c.name END) ILIKE ",
        )
        .push_bind(format!("%{}%", escape_like(q)));
    }
    if let Some(ns) = &opts.namespaces {
        b.push(" AND (false");
        for n in ns {
            match n {
                None => b.push(" OR (c.namespace_user_id IS NULL AND c.namespace_org_id IS NULL)"),
                Some(Namespace::User(u)) => b.push(" OR c.namespace_user_id = ").push_bind(*u),
                Some(Namespace::Org(o)) => b.push(" OR c.namespace_org_id = ").push_bind(*o),
            };
        }
        b.push(")");
    }
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn set_repos(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    input: &SearchContextInput,
) -> Result<()> {
    if input.query.is_some() {
        return Ok(());
    }
    for (repo, revs) in &input.repos {
        let revs: Vec<&str> = if revs.is_empty() {
            vec![""]
        } else {
            revs.iter().map(String::as_str).collect()
        };
        for rev in revs {
            sqlx::query(
                "INSERT INTO search_context_repos (search_context_id, repo_id, revision)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(*repo)
            .bind(rev)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// `saved_searches.visibility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedSearchVisibility {
    Secret,
    Public,
}

impl SavedSearchVisibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Secret => "secret",
            Self::Public => "public",
        }
    }
}

/// A saved search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: i64,
    pub description: String,
    pub query: String,
    pub owner: Namespace,
    pub draft: bool,
    pub visibility: SavedSearchVisibility,
    pub created_by: Option<UserId>,
    pub updated_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct SavedSearchRow {
    id: i64,
    description: String,
    query: String,
    owner_user_id: Option<UserId>,
    owner_org_id: Option<OrgId>,
    draft: bool,
    visibility: String,
    created_by: Option<UserId>,
    updated_by: Option<UserId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<SavedSearchRow> for SavedSearch {
    type Error = DbError;

    fn try_from(r: SavedSearchRow) -> Result<Self> {
        Ok(SavedSearch {
            id: r.id,
            description: r.description,
            query: r.query,
            owner: Namespace::from_columns(r.owner_user_id, r.owner_org_id)
                .ok_or(DbError::Invalid("saved search without an owner"))?,
            draft: r.draft,
            visibility: if r.visibility == "public" {
                SavedSearchVisibility::Public
            } else {
                SavedSearchVisibility::Secret
            },
            created_by: r.created_by,
            updated_by: r.updated_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

const SAVED_COLUMNS: &str = "id, description, query, owner_user_id, owner_org_id, draft,
    visibility, created_by, updated_by, created_at, updated_at";

/// How saved searches are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SavedSearchOrder {
    #[default]
    UpdatedAt,
    Description,
}

/// Which saved searches to list.
#[derive(Debug, Clone, Default)]
pub struct ListSavedSearchesOptions {
    /// Case-insensitive substring of the description or query.
    pub query: Option<String>,
    /// Only this owner's.
    pub owner: Option<Namespace>,
    /// Only this user's, their orgs' and public ones (`viewerIsAffiliated`).
    pub affiliated_user: Option<UserId>,
    /// Only public ones.
    pub public_only: bool,
    pub include_drafts: bool,
    pub order: SavedSearchOrder,
    pub limit: i64,
    pub offset: i64,
}

/// Reads and writes saved searches.
#[derive(Debug, Clone)]
pub struct SavedSearchStore {
    pool: PgPool,
}

fn push_owners(b: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>, owners: &[Namespace]) {
    b.push("(false");
    for n in owners {
        match n {
            Namespace::User(u) => b.push(" OR owner_user_id = ").push_bind(*u),
            Namespace::Org(o) => b.push(" OR owner_org_id = ").push_bind(*o),
        };
    }
    b.push(")");
}

impl SavedSearchStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        owner: Namespace,
        description: &str,
        query: &str,
        draft: bool,
        by: UserId,
    ) -> Result<SavedSearch> {
        let (user, org) = Namespace::columns(Some(owner));
        let row: SavedSearchRow = sqlx::query_as(&format!(
            "INSERT INTO saved_searches
                (description, query, owner_user_id, owner_org_id, draft, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING {SAVED_COLUMNS}"
        ))
        .bind(description)
        .bind(query)
        .bind(user)
        .bind(org)
        .bind(draft)
        .bind(by)
        .fetch_one(&self.pool)
        .await?;
        row.try_into()
    }

    pub async fn get(&self, id: i64) -> Result<Option<SavedSearch>> {
        let row: Option<SavedSearchRow> = sqlx::query_as(&format!(
            "SELECT {SAVED_COLUMNS} FROM saved_searches WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub async fn update(
        &self,
        id: i64,
        description: &str,
        query: &str,
        draft: bool,
        by: UserId,
    ) -> Result<SavedSearch> {
        let row: Option<SavedSearchRow> = sqlx::query_as(&format!(
            "UPDATE saved_searches SET description = $2, query = $3, draft = $4,
                updated_by = $5, updated_at = now()
             WHERE id = $1 RETURNING {SAVED_COLUMNS}"
        ))
        .bind(id)
        .bind(description)
        .bind(query)
        .bind(draft)
        .bind(by)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(DbError::NotFound("saved search"))?.try_into()
    }

    /// Moves a saved search to another owner.
    pub async fn transfer(&self, id: i64, owner: Namespace, by: UserId) -> Result<SavedSearch> {
        let (user, org) = Namespace::columns(Some(owner));
        let row: Option<SavedSearchRow> = sqlx::query_as(&format!(
            "UPDATE saved_searches SET owner_user_id = $2, owner_org_id = $3, updated_by = $4,
                updated_at = now()
             WHERE id = $1 RETURNING {SAVED_COLUMNS}"
        ))
        .bind(id)
        .bind(user)
        .bind(org)
        .bind(by)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(DbError::NotFound("saved search"))?.try_into()
    }

    pub async fn set_visibility(
        &self,
        id: i64,
        visibility: SavedSearchVisibility,
        by: UserId,
    ) -> Result<SavedSearch> {
        let row: Option<SavedSearchRow> = sqlx::query_as(&format!(
            "UPDATE saved_searches SET visibility = $2, updated_by = $3, updated_at = now()
             WHERE id = $1 RETURNING {SAVED_COLUMNS}"
        ))
        .bind(id)
        .bind(visibility.as_str())
        .bind(by)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(DbError::NotFound("saved search"))?.try_into()
    }

    pub async fn delete(&self, id: i64) -> Result<()> {
        let r = sqlx::query("DELETE FROM saved_searches WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("saved search"));
        }
        Ok(())
    }

    /// Saved searches and the total count.
    pub async fn list(&self, opts: &ListSavedSearchesOptions) -> Result<(Vec<SavedSearch>, i64)> {
        let mut b = sqlx::QueryBuilder::new(format!(
            "SELECT {SAVED_COLUMNS} FROM saved_searches WHERE true"
        ));
        let mut count = sqlx::QueryBuilder::new("SELECT count(*) FROM saved_searches WHERE true");
        for q in [&mut b, &mut count] {
            if let Some(text) = &opts.query {
                let like = format!("%{}%", escape_like(text));
                q.push(" AND (description ILIKE ")
                    .push_bind(like.clone())
                    .push(" OR query ILIKE ")
                    .push_bind(like)
                    .push(")");
            }
            if let Some(owner) = opts.owner {
                q.push(" AND ");
                push_owners(q, &[owner]);
            }
            if let Some(user) = opts.affiliated_user {
                q.push(" AND (visibility = 'public' OR owner_user_id = ")
                    .push_bind(user)
                    .push(
                        " OR owner_org_id IN (SELECT m.org_id FROM org_members m
                         JOIN orgs o ON o.id = m.org_id
                         WHERE o.deleted_at IS NULL AND m.user_id = ",
                    )
                    .push_bind(user)
                    .push("))");
            }
            if opts.public_only {
                q.push(" AND visibility = 'public'");
            }
            if !opts.include_drafts {
                q.push(" AND NOT draft");
            }
        }
        b.push(match opts.order {
            SavedSearchOrder::UpdatedAt => " ORDER BY updated_at DESC, id DESC",
            SavedSearchOrder::Description => " ORDER BY lower(description), id",
        })
        .push(" LIMIT ")
        .push_bind(opts.limit)
        .push(" OFFSET ")
        .push_bind(opts.offset);
        let rows: Vec<SavedSearchRow> = b.build_query_as().fetch_all(&self.pool).await?;
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;
        Ok((
            rows.into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
            total,
        ))
    }
}
