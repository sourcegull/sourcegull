//! `batch_changes`, and [`ApplyTx`]: the transaction applying a bulk change spec runs in.
//!
//! A bulk change is unique by name in its namespace. One created for server-side execution
//! before its first apply is a draft (`last_applied_at` is NULL). Deleting a bulk change
//! removes it from its changesets' `batch_change_ids` (trigger).

use chrono::{DateTime, Utc};
use sourcegull_db::{Db, DbError, Namespace, Result};
use sourcegull_types::{OrgId, RepoId, UserId};
use sqlx::{PgPool, Postgres, Transaction};

use super::changesets::{self, AssocChange};
use super::{Changeset, NewChangeset, UiPublicationState, str_enum};

str_enum!(
    /// The state of a bulk change.
    BatchChangeState {
        Open => "OPEN",
        Closed => "CLOSED",
        /// Created for server-side execution and never applied.
        Draft => "DRAFT",
    }
);

/// A bulk change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchChange {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub namespace: Namespace,
    pub creator_id: Option<UserId>,
    pub last_applier_id: Option<UserId>,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    /// The last applied spec (or, for a draft, the spec it was created with).
    pub batch_spec_id: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BatchChange {
    /// The bulk change's state.
    pub fn state(&self) -> BatchChangeState {
        if self.closed_at.is_some() {
            BatchChangeState::Closed
        } else if self.last_applied_at.is_none() {
            BatchChangeState::Draft
        } else {
            BatchChangeState::Open
        }
    }
}

/// A bulk change to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBatchChange {
    pub name: String,
    pub description: String,
    pub namespace: Namespace,
    pub creator_id: Option<UserId>,
    pub batch_spec_id: i64,
    /// Created by an apply (the creator is the last applier); otherwise a draft.
    pub applied: bool,
}

/// Filters of [`BatchChangeStore::list`]. Bulk changes are listed newest first.
#[derive(Debug, Clone, Default)]
pub struct ListBatchChanges {
    pub namespace: Option<Namespace>,
    /// Any state when empty.
    pub states: Vec<BatchChangeState>,
    /// Only bulk changes tracking a changeset in this repository.
    pub repo: Option<RepoId>,
    /// Only bulk changes created by this user.
    pub creator: Option<UserId>,
    /// Leave out drafts (never applied) not created by this user (what non-admins see).
    pub exclude_drafts_not_owned_by: Option<UserId>,
    /// Only bulk changes in this user's namespace or in an organization they belong to
    /// (`viewerCanAdminister`).
    pub administered_by: Option<UserId>,
    /// Only bulk changes tracking this changeset.
    pub changeset_id: Option<i64>,
    /// Only bulk changes with an id below this cursor.
    pub before_id: Option<i64>,
    /// Page size; no limit when 0.
    pub limit: i64,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    name: String,
    description: String,
    namespace_user_id: Option<UserId>,
    namespace_org_id: Option<OrgId>,
    creator_id: Option<UserId>,
    last_applier_id: Option<UserId>,
    last_applied_at: Option<DateTime<Utc>>,
    closed_at: Option<DateTime<Utc>>,
    batch_spec_id: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<Row> for BatchChange {
    type Error = DbError;

    fn try_from(r: Row) -> Result<Self> {
        Ok(Self {
            id: r.id,
            name: r.name,
            description: r.description,
            namespace: Namespace::from_columns(r.namespace_user_id, r.namespace_org_id)
                .ok_or(DbError::Invalid("bulk change without namespace"))?,
            creator_id: r.creator_id,
            last_applier_id: r.last_applier_id,
            last_applied_at: r.last_applied_at,
            closed_at: r.closed_at,
            batch_spec_id: r.batch_spec_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// `WHERE` clause of [`ListBatchChanges`] over `batch_changes b`, binds `$1`..`$9`.
const LIST_FILTER: &str = "($1::bigint IS NULL OR b.namespace_user_id = $1)
    AND ($2::bigint IS NULL OR b.namespace_org_id = $2)
    AND (cardinality($3::text[]) = 0
         OR (CASE WHEN b.closed_at IS NOT NULL THEN 'CLOSED'
                  WHEN b.last_applied_at IS NULL THEN 'DRAFT' ELSE 'OPEN' END) = ANY($3))
    AND ($4::int IS NULL OR EXISTS (
        SELECT 1 FROM changesets c
        WHERE c.repo_id = $4 AND c.batch_change_ids ? b.id::text))
    AND ($5::bigint IS NULL OR b.creator_id = $5)
    AND ($6::bigint IS NULL OR b.id < $6)
    AND ($7::bigint IS NULL OR b.last_applied_at IS NOT NULL OR b.creator_id = $7)
    AND ($8::bigint IS NULL OR b.namespace_user_id = $8 OR EXISTS (
        SELECT 1 FROM org_members m JOIN orgs o ON o.id = m.org_id AND o.deleted_at IS NULL
        WHERE m.org_id = b.namespace_org_id AND m.user_id = $8))
    AND ($9::bigint IS NULL OR EXISTS (
        SELECT 1 FROM changesets c WHERE c.id = $9 AND c.batch_change_ids ? b.id::text))";

async fn insert(conn: &mut sqlx::PgConnection, new: &NewBatchChange) -> Result<BatchChange> {
    let (user, org) = Namespace::columns(Some(new.namespace));
    let row: Row = sqlx::query_as(
        "INSERT INTO batch_changes (name, description, namespace_user_id, namespace_org_id,
             creator_id, last_applier_id, last_applied_at, batch_spec_id)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN $5 END, CASE WHEN $6 THEN now() END, $7)
         RETURNING *",
    )
    .bind(&new.name)
    .bind(&new.description)
    .bind(user)
    .bind(org)
    .bind(new.creator_id)
    .bind(new.applied)
    .bind(new.batch_spec_id)
    .fetch_one(&mut *conn)
    .await?;
    row.try_into()
}

/// Store for bulk changes.
#[derive(Debug, Clone)]
pub struct BatchChangeStore {
    pool: PgPool,
}

impl BatchChangeStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// Starts the transaction of an apply.
    pub async fn begin_apply(&self) -> Result<ApplyTx> {
        Ok(ApplyTx {
            tx: self.pool.begin().await?,
        })
    }

    /// Creates a bulk change (a draft unless `applied`). A taken name fails with
    /// [`DbError::AlreadyExists`].
    pub async fn create(&self, new: &NewBatchChange) -> Result<BatchChange> {
        let mut conn = self.pool.acquire().await?;
        insert(&mut conn, new).await
    }

    /// Looks up a bulk change.
    pub async fn get(&self, id: i64) -> Result<Option<BatchChange>> {
        let row: Option<Row> = sqlx::query_as("SELECT * FROM batch_changes WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Looks up a bulk change by namespace and name.
    pub async fn get_by_name(
        &self,
        namespace: Namespace,
        name: &str,
    ) -> Result<Option<BatchChange>> {
        let (user, org) = Namespace::columns(Some(namespace));
        let row: Option<Row> = sqlx::query_as(
            "SELECT * FROM batch_changes
             WHERE name = $1 AND (namespace_user_id = $2 OR namespace_org_id = $3)",
        )
        .bind(name)
        .bind(user)
        .bind(org)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Bulk changes matching `opts`, newest first.
    pub async fn list(&self, opts: &ListBatchChanges) -> Result<Vec<BatchChange>> {
        let (user, org) = Namespace::columns(opts.namespace);
        let states: Vec<&str> = opts.states.iter().map(|s| s.as_str()).collect();
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT b.* FROM batch_changes b WHERE {LIST_FILTER} ORDER BY b.id DESC
             LIMIT CASE WHEN $10::bigint > 0 THEN $10::bigint END"
        ))
        .bind(user)
        .bind(org)
        .bind(&states)
        .bind(opts.repo.map(|r| r.get()))
        .bind(opts.creator)
        .bind(opts.before_id)
        .bind(opts.exclude_drafts_not_owned_by)
        .bind(opts.administered_by)
        .bind(opts.changeset_id)
        .bind(opts.limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// The number of bulk changes matching `opts` (ignoring `before_id` and `limit`).
    pub async fn count(&self, opts: &ListBatchChanges) -> Result<i64> {
        let (user, org) = Namespace::columns(opts.namespace);
        let states: Vec<&str> = opts.states.iter().map(|s| s.as_str()).collect();
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM batch_changes b WHERE {LIST_FILTER}"
        ))
        .bind(user)
        .bind(org)
        .bind(&states)
        .bind(opts.repo.map(|r| r.get()))
        .bind(opts.creator)
        .bind(None::<i64>)
        .bind(opts.exclude_drafts_not_owned_by)
        .bind(opts.administered_by)
        .bind(opts.changeset_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Updates the description of a bulk change.
    pub async fn set_description(&self, id: i64, description: &str) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE batch_changes SET description = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(description)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Closes a bulk change (keeping the first close time). With `close_changesets`, its
    /// open changesets it created are marked for closing and queued. Returns the bulk change.
    pub async fn close(&self, id: i64, close_changesets: bool) -> Result<Option<BatchChange>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<Row> = sqlx::query_as(
            "UPDATE batch_changes SET closed_at = COALESCE(closed_at, now()), updated_at = now()
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if row.is_some() && close_changesets {
            let ids: Vec<i64> = sqlx::query_scalar(
                "UPDATE changesets SET closing = true, updated_at = now()
                 WHERE owned_by_batch_change_id = $1 AND computed_state IN ('OPEN', 'DRAFT')
                   AND publication_state = 'PUBLISHED'
                 RETURNING id",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            changesets::enqueue(&mut tx, &ids, false).await?;
        }
        tx.commit().await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Deletes a bulk change (its changesets stay, untracked by it, and its bulk jobs go).
    pub async fn delete(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE batch_specs SET batch_change_id = NULL WHERE batch_change_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let n = sqlx::query("DELETE FROM batch_changes WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }
}

/// The transaction an apply runs in: everything it does becomes visible (and the reconciler
/// is woken) on [`commit`](Self::commit); dropping it rolls back.
pub struct ApplyTx {
    tx: Transaction<'static, Postgres>,
}

impl ApplyTx {
    /// Looks up the bulk change of a namespace and name and locks it against concurrent
    /// applies.
    pub async fn lock_batch_change(
        &mut self,
        namespace: Namespace,
        name: &str,
    ) -> Result<Option<BatchChange>> {
        let (user, org) = Namespace::columns(Some(namespace));
        let row: Option<Row> = sqlx::query_as(
            "SELECT * FROM batch_changes
             WHERE name = $1 AND (namespace_user_id = $2 OR namespace_org_id = $3) FOR UPDATE",
        )
        .bind(name)
        .bind(user)
        .bind(org)
        .fetch_optional(&mut *self.tx)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Creates the bulk change.
    pub async fn create_batch_change(&mut self, new: &NewBatchChange) -> Result<BatchChange> {
        insert(&mut self.tx, new).await
    }

    /// Records an apply of `batch_spec_id` by `applier`: the spec becomes current, the
    /// description is replaced and a closed bulk change opens again.
    pub async fn record_apply(
        &mut self,
        batch_change_id: i64,
        batch_spec_id: i64,
        description: &str,
        applier: UserId,
    ) -> Result<BatchChange> {
        let row: Option<Row> = sqlx::query_as(
            "UPDATE batch_changes SET batch_spec_id = $2, description = $3, last_applier_id = $4,
                 last_applied_at = now(), closed_at = NULL, updated_at = now()
             WHERE id = $1 RETURNING *",
        )
        .bind(batch_change_id)
        .bind(batch_spec_id)
        .bind(description)
        .bind(applier)
        .fetch_optional(&mut *self.tx)
        .await?;
        sqlx::query("UPDATE batch_specs SET batch_change_id = $1 WHERE id = $2")
            .bind(batch_change_id)
            .bind(batch_spec_id)
            .execute(&mut *self.tx)
            .await?;
        row.ok_or(DbError::NotFound("bulk change"))?.try_into()
    }

    /// The bulk change's changesets, locked for the rest of the apply.
    pub async fn changesets(&mut self, batch_change_id: i64) -> Result<Vec<Changeset>> {
        changesets::of_batch_change_for_update(&mut self.tx, batch_change_id).await
    }

    /// The changesets an apply maps the bulk change spec's changeset specs to, locked: those
    /// tracked or created by `batch_change_id` and those of the `imports` (repository and
    /// external id pairs), in id order.
    pub async fn mapping_candidates(
        &mut self,
        batch_change_id: Option<i64>,
        imports: &[(RepoId, String)],
    ) -> Result<Vec<Changeset>> {
        changesets::mapping_candidates(&mut self.tx, batch_change_id, imports, true).await
    }

    /// Creates a changeset (queued for the reconciler on commit).
    pub async fn create_changeset(&mut self, new: &NewChangeset) -> Result<Changeset> {
        changesets::create(&mut self.tx, new).await
    }

    /// Points a changeset at a new spec and cancels a pending close; the current spec becomes
    /// the previous one if the reconciler completed it.
    pub async fn set_changeset_spec(&mut self, changeset_id: i64, spec_id: i64) -> Result<bool> {
        changesets::set_spec(&mut self.tx, changeset_id, spec_id).await
    }

    /// Sets the publication chosen in the UI (`publicationStates`).
    pub async fn set_ui_publication_state(
        &mut self,
        changeset_id: i64,
        state: Option<UiPublicationState>,
    ) -> Result<bool> {
        changesets::set_ui_publication_state(&mut self.tx, changeset_id, state).await
    }

    /// Tracks changesets in the bulk change with no pending request (attaches, re-attaches
    /// and un-archives).
    pub async fn attach(&mut self, batch_change_id: i64, changeset_ids: &[i64]) -> Result<u64> {
        changesets::set_assoc(
            &mut self.tx,
            batch_change_id,
            changeset_ids,
            AssocChange::Attach,
        )
        .await
    }

    /// Requests detaching changesets (imported ones dropped from the spec).
    pub async fn detach(&mut self, batch_change_id: i64, changeset_ids: &[i64]) -> Result<u64> {
        changesets::set_assoc(
            &mut self.tx,
            batch_change_id,
            changeset_ids,
            AssocChange::Detach,
        )
        .await
    }

    /// Requests archiving changesets (created ones dropped from the spec, closed by the
    /// reconciler first when `close`).
    pub async fn archive(
        &mut self,
        batch_change_id: i64,
        changeset_ids: &[i64],
        close: bool,
    ) -> Result<u64> {
        if close {
            sqlx::query(
                "UPDATE changesets SET closing = true, updated_at = now()
                 WHERE id = ANY($1) AND publication_state = 'PUBLISHED'
                   AND computed_state NOT IN ('CLOSED', 'MERGED', 'DELETED')",
            )
            .bind(changeset_ids)
            .execute(&mut *self.tx)
            .await?;
        }
        changesets::set_assoc(
            &mut self.tx,
            batch_change_id,
            changeset_ids,
            AssocChange::Archive,
        )
        .await
    }

    /// Queues changesets for the reconciler (`scheduled` for rollout windows).
    pub async fn enqueue(&mut self, changeset_ids: &[i64], scheduled: bool) -> Result<u64> {
        changesets::enqueue(&mut self.tx, changeset_ids, scheduled).await
    }

    /// The underlying transaction, for statements the methods above do not cover.
    pub fn conn(&mut self) -> &mut sqlx::PgConnection {
        &mut self.tx
    }

    /// Commits the apply.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}
