//! The notebooks store: notebooks, stars, listing with full-text search.
//!
//! Every read takes an [`Access`] and applies the visibility rule in SQL: public
//! notebooks are visible to everyone, private ones to the owning user or the owning org's
//! members, and nobody else (site admins included). Write permissions are the API's.

use chrono::{DateTime, Utc};
use sourcegull_db::{DbError, Namespace};
use sourcegull_types::{OrgId, UserId};
use sqlx::types::Json;
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::blocks::Block;

type Result<T> = std::result::Result<T, DbError>;

/// A notebook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notebook {
    pub id: i64,
    pub title: String,
    pub blocks: Vec<Block>,
    pub public: bool,
    /// `None` once the creator is deleted.
    pub creator: Option<UserId>,
    pub updater: Option<UserId>,
    pub namespace: Namespace,
    /// The pattern type of queries without `patternType:`.
    pub pattern_type: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A user's star on a notebook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookStar {
    pub notebook_id: i64,
    pub user_id: UserId,
    pub created_at: DateTime<Utc>,
}

/// Who reads notebooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// A user, or anonymous (`None`): public notebooks, plus private ones of the user and of
    /// their orgs.
    Viewer(Option<UserId>),
    /// Internal callers: every notebook.
    Internal,
}

/// The fields a notebook is created or updated with.
#[derive(Debug, Clone)]
pub struct NotebookInput {
    pub title: String,
    pub blocks: Vec<Block>,
    pub public: bool,
    pub namespace: Namespace,
}

/// How notebooks are ordered (ties broken by ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotebookOrder {
    #[default]
    UpdatedAt,
    CreatedAt,
    StarCount,
}

/// Which notebooks to list.
#[derive(Debug, Clone)]
pub struct ListNotebooks {
    pub access: Access,
    /// Words matched against the title (substring) and, as prefixes, the full-text index of
    /// the title and the Markdown and query blocks.
    pub query: Option<String>,
    pub creator: Option<UserId>,
    pub starred_by: Option<UserId>,
    pub namespace: Option<Namespace>,
    pub order: NotebookOrder,
    pub descending: bool,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ListNotebooks {
    fn default() -> Self {
        Self {
            access: Access::Viewer(None),
            query: None,
            creator: None,
            starred_by: None,
            namespace: None,
            order: NotebookOrder::default(),
            descending: false,
            limit: 50,
            offset: 0,
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    title: String,
    blocks: Json<Vec<Block>>,
    public: bool,
    creator_user_id: Option<UserId>,
    updater_user_id: Option<UserId>,
    namespace_user_id: Option<UserId>,
    namespace_org_id: Option<OrgId>,
    pattern_type: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<Row> for Notebook {
    type Error = DbError;

    fn try_from(r: Row) -> Result<Self> {
        let namespace = match (r.namespace_user_id, r.namespace_org_id) {
            (Some(u), _) => Namespace::User(u),
            (None, Some(o)) => Namespace::Org(o),
            (None, None) => return Err(DbError::Invalid("notebook without a namespace")),
        };
        Ok(Notebook {
            id: r.id,
            title: r.title,
            blocks: r.blocks.0,
            public: r.public,
            creator: r.creator_user_id,
            updater: r.updater_user_id,
            namespace,
            pattern_type: r.pattern_type,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

const COLUMNS: &str = "n.id, n.title, n.blocks, n.public, n.creator_user_id, n.updater_user_id,
    n.namespace_user_id, n.namespace_org_id, n.pattern_type, n.created_at, n.updated_at";

/// Appends ` AND <visible to access>` (deleted orgs' memberships are ignored).
fn push_access(b: &mut QueryBuilder<'_, Postgres>, access: Access) {
    let Access::Viewer(user) = access else {
        return;
    };
    b.push(" AND (n.public");
    if let Some(user) = user {
        b.push(" OR n.namespace_user_id = ")
            .push_bind(user)
            .push(
                " OR (n.namespace_org_id IS NOT NULL AND EXISTS (
                    SELECT 1 FROM org_members m JOIN orgs o ON o.id = m.org_id
                    WHERE m.org_id = n.namespace_org_id AND o.deleted_at IS NULL
                      AND m.user_id = ",
            )
            .push_bind(user)
            .push("))");
    }
    b.push(")");
}

/// The prefix `tsquery` for `query`: each word becomes `word:*`, all
/// required. Only letters, digits and `_` are kept, so no input is a `tsquery` syntax error.
/// `None` when no word is left.
pub fn to_tsquery(query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| format!("{w}:*"))
        .collect();
    (!words.is_empty()).then(|| words.join(" & "))
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

fn push_filters(b: &mut QueryBuilder<'_, Postgres>, opts: &ListNotebooks) {
    push_access(b, opts.access);
    if let Some(q) = opts
        .query
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        b.push(" AND (n.title ILIKE ")
            .push_bind(format!("%{}%", escape_like(q)));
        if let Some(ts) = to_tsquery(q) {
            b.push(" OR n.content_tsvector @@ to_tsquery('english', ")
                .push_bind(ts)
                .push(")");
        }
        b.push(")");
    }
    if let Some(u) = opts.creator {
        b.push(" AND n.creator_user_id = ").push_bind(u);
    }
    match opts.namespace {
        Some(Namespace::User(u)) => {
            b.push(" AND n.namespace_user_id = ").push_bind(u);
        }
        Some(Namespace::Org(o)) => {
            b.push(" AND n.namespace_org_id = ").push_bind(o);
        }
        None => {}
    }
    if let Some(u) = opts.starred_by {
        b.push(" AND EXISTS (SELECT 1 FROM notebook_stars s WHERE s.notebook_id = n.id AND s.user_id = ")
            .push_bind(u)
            .push(")");
    }
}

/// Reads and writes notebooks and their stars.
#[derive(Debug, Clone)]
pub struct NotebookStore {
    pool: PgPool,
}

impl NotebookStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The notebook `id` if `access` may see it.
    pub async fn get(&self, id: i64, access: Access) -> Result<Option<Notebook>> {
        let mut b = QueryBuilder::new(format!("SELECT {COLUMNS} FROM notebooks n WHERE n.id = "));
        b.push_bind(id);
        push_access(&mut b, access);
        let row: Option<Row> = b.build_query_as().fetch_optional(&self.pool).await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Creates a notebook by `by`. The caller checks permissions and validates the blocks.
    pub async fn create(&self, input: &NotebookInput, by: UserId) -> Result<Notebook> {
        let (user, org) = columns(input.namespace);
        let row: Row = sqlx::query_as(&format!(
            "INSERT INTO notebooks AS n
                (title, blocks, public, creator_user_id, updater_user_id, namespace_user_id,
                 namespace_org_id)
             VALUES ($1, $2, $3, $4, $4, $5, $6) RETURNING {COLUMNS}"
        ))
        .bind(&input.title)
        .bind(Json(&input.blocks))
        .bind(input.public)
        .bind(by)
        .bind(user)
        .bind(org)
        .fetch_one(&self.pool)
        .await?;
        row.try_into()
    }

    /// Replaces a notebook's fields. The caller checks permissions.
    pub async fn update(&self, id: i64, input: &NotebookInput, by: UserId) -> Result<Notebook> {
        let (user, org) = columns(input.namespace);
        let row: Option<Row> = sqlx::query_as(&format!(
            "UPDATE notebooks AS n SET title = $2, blocks = $3, public = $4, updater_user_id = $5,
                namespace_user_id = $6, namespace_org_id = $7, updated_at = now()
             WHERE id = $1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(&input.title)
        .bind(Json(&input.blocks))
        .bind(input.public)
        .bind(by)
        .bind(user)
        .bind(org)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(DbError::NotFound("notebook"))?.try_into()
    }

    /// Deletes a notebook (and its stars). The caller checks permissions.
    pub async fn delete(&self, id: i64) -> Result<()> {
        let r = sqlx::query("DELETE FROM notebooks WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(DbError::NotFound("notebook"));
        }
        Ok(())
    }

    /// A page of the notebooks matching `opts`, and their total count.
    pub async fn list(&self, opts: &ListNotebooks) -> Result<(Vec<Notebook>, i64)> {
        let mut b = QueryBuilder::new(format!("SELECT {COLUMNS} FROM notebooks n WHERE true"));
        push_filters(&mut b, opts);
        let dir = if opts.descending { "DESC" } else { "ASC" };
        b.push(match opts.order {
            NotebookOrder::UpdatedAt => format!(" ORDER BY n.updated_at {dir}, n.id {dir}"),
            NotebookOrder::CreatedAt => format!(" ORDER BY n.created_at {dir}, n.id {dir}"),
            NotebookOrder::StarCount => format!(
                " ORDER BY (SELECT count(*) FROM notebook_stars s WHERE s.notebook_id = n.id) \
                 {dir}, n.id {dir}"
            ),
        })
        .push(" LIMIT ")
        .push_bind(opts.limit)
        .push(" OFFSET ")
        .push_bind(opts.offset);
        let rows: Vec<Row> = b.build_query_as().fetch_all(&self.pool).await?;

        let mut count = QueryBuilder::new("SELECT count(*) FROM notebooks n WHERE true");
        push_filters(&mut count, opts);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;
        Ok((
            rows.into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_>>()?,
            total,
        ))
    }

    /// Stars notebook `id` for `user`; starring twice keeps the first star. The caller checks
    /// that the user may see the notebook.
    pub async fn create_star(&self, id: i64, user: UserId) -> Result<NotebookStar> {
        sqlx::query(
            "INSERT INTO notebook_stars (notebook_id, user_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(user)
        .execute(&self.pool)
        .await?;
        self.get_star(id, user)
            .await?
            .ok_or(DbError::NotFound("notebook"))
    }

    /// Removes `user`'s star, if any.
    pub async fn delete_star(&self, id: i64, user: UserId) -> Result<()> {
        sqlx::query("DELETE FROM notebook_stars WHERE notebook_id = $1 AND user_id = $2")
            .bind(id)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// `user`'s star on notebook `id`.
    pub async fn get_star(&self, id: i64, user: UserId) -> Result<Option<NotebookStar>> {
        let row: Option<(i64, UserId, DateTime<Utc>)> = sqlx::query_as(
            "SELECT notebook_id, user_id, created_at FROM notebook_stars
             WHERE notebook_id = $1 AND user_id = $2",
        )
        .bind(id)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(notebook_id, user_id, created_at)| NotebookStar {
            notebook_id,
            user_id,
            created_at,
        }))
    }

    /// A page of notebook `id`'s stars (newest first) by live users, and their total count.
    pub async fn list_stars(
        &self,
        id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<NotebookStar>, i64)> {
        let rows: Vec<(i64, UserId, DateTime<Utc>)> = sqlx::query_as(
            "SELECT s.notebook_id, s.user_id, s.created_at FROM notebook_stars s
             JOIN users u ON u.id = s.user_id AND u.deleted_at IS NULL
             WHERE s.notebook_id = $1
             ORDER BY s.created_at DESC, s.user_id DESC LIMIT $2 OFFSET $3",
        )
        .bind(id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM notebook_stars s
             JOIN users u ON u.id = s.user_id AND u.deleted_at IS NULL
             WHERE s.notebook_id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok((
            rows.into_iter()
                .map(|(notebook_id, user_id, created_at)| NotebookStar {
                    notebook_id,
                    user_id,
                    created_at,
                })
                .collect(),
            total,
        ))
    }
}

fn columns(ns: Namespace) -> (Option<UserId>, Option<OrgId>) {
    match ns {
        Namespace::User(u) => (Some(u), None),
        Namespace::Org(o) => (None, Some(o)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsquery_from_words() {
        assert_eq!(to_tsquery("foo"), Some("foo:*".into()));
        assert_eq!(
            to_tsquery(" foo & (bar|baz) ':*! x\\y "),
            Some("foo:* & bar:* & baz:* & x:* & y:*".into())
        );
        assert_eq!(to_tsquery("&|!():"), None);
        assert_eq!(to_tsquery("überall_2"), Some("überall_2:*".into()));
    }
}
