//! `changesets` and `changeset_events`.
//!
//! A changeset row is also the reconciler's queue entry: the standard pgqueue columns, with
//! `state` as the reconciler state plus `scheduled` (waiting for a rollout window). Workers
//! dequeue with `Queue::<Changeset>::new(pool, ChangesetStore::queue_config())`; a dequeued
//! row is always `processing`, so it decodes as a `Record` (`Queue::get` on a scheduled row
//! does not: use [`ChangesetStore::get`]). Every re-enqueue resets the failure count.
//!
//! Rollout windows release `scheduled` changesets oldest update first
//! ([`ChangesetStore::release_scheduled`]), marking them `window_released` so the reconciler
//! publishes them instead of scheduling them again; a re-enqueue clears the mark.
//!
//! Membership in bulk changes is the `batch_change_ids` object, `{"<id>": {"detach",
//! "archive", "isArchived"}}`: `detach`/`archive` are requests the reconciler carries out,
//! after which the entry is removed or marked archived.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sourcegull_db::{Db, Fence, Result};
use sourcegull_pgqueue::{ExecutionLogEntry, QueueConfig, notify_enqueued};
use sourcegull_types::{RepoAccess, RepoId};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Row};

use super::{
    ChangesetState, CheckState, ExternalState, PublicationState, ReconcilerState, ReviewState,
    UiPublicationState, search_terms,
};

/// The reconciler's queue table.
pub const CHANGESETS_TABLE: &str = "changesets";

/// A changeset's membership in one bulk change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchChangeAssoc {
    /// The reconciler should detach the changeset from the bulk change.
    pub detach: bool,
    /// The reconciler should archive the changeset in the bulk change.
    pub archive: bool,
    /// Archived: hidden from the bulk change's default listing.
    #[serde(rename = "isArchived")]
    pub is_archived: bool,
}

/// A changeset.
#[derive(Debug, Clone, PartialEq)]
pub struct Changeset {
    pub id: i64,
    pub repo_id: RepoId,
    pub batch_change_ids: BTreeMap<i64, BatchChangeAssoc>,
    pub owned_by_batch_change_id: Option<i64>,
    pub current_spec_id: Option<i64>,
    pub previous_spec_id: Option<i64>,
    pub publication_state: PublicationState,
    pub ui_publication_state: Option<UiPublicationState>,
    pub reconciler_state: ReconcilerState,
    pub state: ChangesetState,
    pub num_failures: i32,
    pub failure_message: Option<String>,
    pub process_after: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub execution_logs: Vec<ExecutionLogEntry>,
    pub external_id: Option<String>,
    pub external_service_type: String,
    pub external_branch: Option<String>,
    pub external_fork_namespace: Option<String>,
    pub external_fork_name: Option<String>,
    pub external_state: Option<ExternalState>,
    pub external_review_state: Option<ReviewState>,
    pub external_check_state: Option<CheckState>,
    pub external_updated_at: Option<DateTime<Utc>>,
    pub external_deleted_at: Option<DateTime<Utc>>,
    pub diff_stat_added: Option<i32>,
    pub diff_stat_deleted: Option<i32>,
    /// The merge request as last synced.
    pub metadata: serde_json::Value,
    pub title: Option<String>,
    pub body: Option<String>,
    pub sync_state: serde_json::Value,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub next_sync_at: Option<DateTime<Utc>>,
    pub syncer_error: Option<String>,
    pub detached_at: Option<DateTime<Utc>>,
    pub closing: bool,
    /// A rollout window (or a "publish now") released it from `scheduled`.
    pub window_released: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Changeset {
    /// Whether the changeset is archived (or about to be) in a bulk change.
    pub fn archived_in(&self, batch_change_id: i64) -> bool {
        self.batch_change_ids
            .get(&batch_change_id)
            .is_some_and(|a| a.archive || a.is_archived)
    }
}

fn decode<T>(
    index: &str,
    value: Option<String>,
    parse: fn(&str) -> Option<T>,
) -> sqlx::Result<Option<T>> {
    value
        .map(|v| {
            parse(&v).ok_or_else(|| sqlx::Error::ColumnDecode {
                index: index.into(),
                source: format!("unknown value {v:?}").into(),
            })
        })
        .transpose()
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for Changeset {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        let Json(ids): Json<BTreeMap<String, BatchChangeAssoc>> =
            row.try_get("batch_change_ids")?;
        let state: String = row.try_get("state")?;
        let num_failures: i32 = row.try_get("num_failures")?;
        let Json(execution_logs) = row.try_get("execution_logs")?;
        let Json(metadata) = row.try_get("metadata")?;
        let Json(sync_state) = row.try_get("sync_state")?;
        Ok(Self {
            id: row.try_get("id")?,
            repo_id: RepoId(row.try_get("repo_id")?),
            batch_change_ids: ids
                .into_iter()
                .filter_map(|(k, v)| k.parse().ok().map(|k| (k, v)))
                .collect(),
            owned_by_batch_change_id: row.try_get("owned_by_batch_change_id")?,
            current_spec_id: row.try_get("current_spec_id")?,
            previous_spec_id: row.try_get("previous_spec_id")?,
            publication_state: decode(
                "publication_state",
                Some(row.try_get("publication_state")?),
                PublicationState::parse,
            )?
            .unwrap_or(PublicationState::Unpublished),
            ui_publication_state: decode(
                "ui_publication_state",
                row.try_get("ui_publication_state")?,
                UiPublicationState::parse,
            )?,
            reconciler_state: ReconcilerState::from_queue(&state, num_failures),
            state: decode(
                "computed_state",
                Some(row.try_get("computed_state")?),
                ChangesetState::parse,
            )?
            .unwrap_or(ChangesetState::Processing),
            num_failures,
            failure_message: row.try_get("failure_message")?,
            process_after: row.try_get("process_after")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            execution_logs,
            external_id: row.try_get("external_id")?,
            external_service_type: row.try_get("external_service_type")?,
            external_branch: row.try_get("external_branch")?,
            external_fork_namespace: row.try_get("external_fork_namespace")?,
            external_fork_name: row.try_get("external_fork_name")?,
            external_state: decode(
                "external_state",
                row.try_get("external_state")?,
                ExternalState::parse,
            )?,
            external_review_state: decode(
                "external_review_state",
                row.try_get("external_review_state")?,
                ReviewState::parse,
            )?,
            external_check_state: decode(
                "external_check_state",
                row.try_get("external_check_state")?,
                CheckState::parse,
            )?,
            external_updated_at: row.try_get("external_updated_at")?,
            external_deleted_at: row.try_get("external_deleted_at")?,
            diff_stat_added: row.try_get("diff_stat_added")?,
            diff_stat_deleted: row.try_get("diff_stat_deleted")?,
            metadata,
            title: row.try_get("title")?,
            body: row.try_get("body")?,
            sync_state,
            last_synced_at: row.try_get("last_synced_at")?,
            next_sync_at: row.try_get("next_sync_at")?,
            syncer_error: row.try_get("syncer_error")?,
            detached_at: row.try_get("detached_at")?,
            closing: row.try_get("closing")?,
            window_released: row.try_get("window_released")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// A changeset to create (on apply, for a new spec or an import).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChangeset {
    pub repo_id: RepoId,
    pub external_service_type: String,
    /// Set for imports (and once published).
    pub external_id: Option<String>,
    pub external_branch: Option<String>,
    pub external_fork_namespace: Option<String>,
    pub external_fork_name: Option<String>,
    /// The bulk change that tracks it (with no pending requests).
    pub batch_change_id: Option<i64>,
    /// The bulk change that created it (`None` for imports).
    pub owned_by_batch_change_id: Option<i64>,
    pub current_spec_id: Option<i64>,
    pub publication_state: PublicationState,
    pub ui_publication_state: Option<UiPublicationState>,
    /// Queue it as `scheduled` (rollout windows) instead of `queued`.
    pub scheduled: bool,
}

impl NewChangeset {
    /// An unpublished changeset of `repo` with nothing else set.
    pub fn new(repo_id: RepoId, external_service_type: impl Into<String>) -> Self {
        Self {
            repo_id,
            external_service_type: external_service_type.into(),
            external_id: None,
            external_branch: None,
            external_fork_namespace: None,
            external_fork_name: None,
            batch_change_id: None,
            owned_by_batch_change_id: None,
            current_spec_id: None,
            publication_state: PublicationState::Unpublished,
            ui_publication_state: None,
            scheduled: false,
        }
    }
}

/// What the reconciler or syncer learned about a changeset from the code host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangesetSync {
    pub external_id: Option<String>,
    pub external_branch: Option<String>,
    pub external_fork_namespace: Option<String>,
    pub external_fork_name: Option<String>,
    pub external_state: Option<ExternalState>,
    pub external_review_state: Option<ReviewState>,
    pub external_check_state: Option<CheckState>,
    pub external_updated_at: Option<DateTime<Utc>>,
    /// Set when the merge request is gone from the code host.
    pub external_deleted_at: Option<DateTime<Utc>>,
    pub diff_stat_added: Option<i32>,
    pub diff_stat_deleted: Option<i32>,
    pub metadata: serde_json::Value,
    pub title: Option<String>,
    pub body: Option<String>,
    pub sync_state: serde_json::Value,
    /// Changes the publication state (the reconciler published it).
    pub publication_state: Option<PublicationState>,
    /// When to sync next.
    pub next_sync_at: Option<DateTime<Utc>>,
}

/// What a code host webhook said about a changeset.
/// `None` fields are left as they are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChangesetHookUpdate {
    pub external_state: Option<ExternalState>,
    pub external_review_state: Option<ReviewState>,
    pub external_check_state: Option<CheckState>,
    pub external_updated_at: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Top-level metadata keys to set; an object merges into an object already there (one
    /// level deep, e.g. changed fields of `mergeRequest`).
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub next_sync_at: Option<DateTime<Utc>>,
}

/// Which archived changesets [`ListChangesets`] includes (only with a bulk change).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ArchivedFilter {
    /// Not archived (the default listing).
    #[default]
    Exclude,
    /// Only archived ones.
    Only,
    /// Both.
    Include,
}

/// Filters of [`ChangesetStore::list`]. Changesets are listed in id order.
#[derive(Debug, Clone, Default)]
pub struct ListChangesets {
    /// Only changesets tracked by this bulk change.
    pub batch_change_id: Option<i64>,
    pub repo: Option<RepoId>,
    pub ids: Option<Vec<i64>>,
    /// UI states (any when empty).
    pub states: Vec<ChangesetState>,
    /// Reconciler states (any when empty).
    pub reconciler_states: Vec<ReconcilerState>,
    pub publication_state: Option<PublicationState>,
    /// External states (any when empty).
    pub external_states: Vec<ExternalState>,
    pub review_state: Option<ReviewState>,
    pub check_state: Option<CheckState>,
    /// Only changesets created by `batch_change_id` (`onlyPublishedByThisBatchChange`).
    pub owned_by_batch_change: bool,
    /// Only open or draft changesets created by `batch_change_id` (`onlyClosable`).
    pub only_closable: bool,
    pub archived: ArchivedFilter,
    /// Terms matched against the title, repository name and external id; `-term` excludes,
    /// `"a phrase"` matches as a whole.
    pub search: Option<String>,
    /// Only changesets with an id above this cursor.
    pub after_id: Option<i64>,
    /// Page size; no limit when 0.
    pub limit: i64,
}

/// Changeset counts of a bulk change by UI state (archived ones only in `archived`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangesetCounts {
    /// All non-archived changesets.
    pub total: i64,
    pub unpublished: i64,
    pub scheduled: i64,
    pub processing: i64,
    pub retrying: i64,
    pub failed: i64,
    pub open: i64,
    pub draft: i64,
    pub closed: i64,
    pub merged: i64,
    pub readonly: i64,
    pub deleted: i64,
    pub archived: i64,
}

impl ChangesetCounts {
    fn add(&mut self, state: ChangesetState, n: i64) {
        self.total += n;
        let slot = match state {
            ChangesetState::Unpublished => &mut self.unpublished,
            ChangesetState::Scheduled => &mut self.scheduled,
            ChangesetState::Processing => &mut self.processing,
            ChangesetState::Retrying => &mut self.retrying,
            ChangesetState::Failed => &mut self.failed,
            ChangesetState::Open => &mut self.open,
            ChangesetState::Draft => &mut self.draft,
            ChangesetState::Closed => &mut self.closed,
            ChangesetState::Merged => &mut self.merged,
            ChangesetState::ReadOnly => &mut self.readonly,
            ChangesetState::Deleted => &mut self.deleted,
        };
        *slot += n;
    }
}

/// A changeset event to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewChangesetEvent {
    /// e.g. `gitlab:approved`, `gitlab:pipeline`, `gitlab:mark_ready`.
    pub kind: String,
    /// Unique per changeset and kind (the code host's note or event id).
    pub key: String,
    /// When it happened on the code host.
    pub created_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

/// A stored changeset event.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ChangesetEvent {
    pub id: i64,
    pub changeset_id: i64,
    pub kind: String,
    pub key: String,
    pub metadata: Json<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The reconciler state of a row `c` as [`ReconcilerState::as_str`].
const RECONCILER_STATE_SQL: &str = "CASE
        WHEN c.state = 'scheduled' THEN 'SCHEDULED'
        WHEN c.state = 'errored' OR (c.state = 'queued' AND c.num_failures > 0) THEN 'ERRORED'
        WHEN c.state = 'queued' THEN 'QUEUED'
        WHEN c.state = 'processing' THEN 'PROCESSING'
        WHEN c.state = 'completed' THEN 'COMPLETED'
        ELSE 'FAILED' END";

/// Whether row `c` is archived (or about to be) in bulk change `$1`.
const ARCHIVED_SQL: &str =
    "COALESCE((c.batch_change_ids -> ($1::bigint)::text ->> 'isArchived')::boolean, false)
    OR COALESCE((c.batch_change_ids -> ($1::bigint)::text ->> 'archive')::boolean, false)";

fn list_from() -> String {
    format!(
        "changesets c
         JOIN repo r ON r.id = c.repo_id AND r.deleted_at IS NULL
         LEFT JOIN changeset_specs s ON s.id = c.current_spec_id
         WHERE ($1::bigint IS NULL OR c.batch_change_ids ? ($1::bigint)::text)
           AND ($2::int IS NULL OR c.repo_id = $2)
           AND ($3::int[] IS NULL OR c.repo_id = ANY($3))
           AND ($4::bigint[] IS NULL OR c.id = ANY($4))
           AND (cardinality($5::text[]) = 0 OR c.computed_state = ANY($5))
           AND (cardinality($6::text[]) = 0 OR ({RECONCILER_STATE_SQL}) = ANY($6))
           AND ($7::text IS NULL OR c.publication_state = $7)
           AND (cardinality($8::text[]) = 0 OR c.external_state = ANY($8))
           AND ($9::text IS NULL OR c.external_review_state = $9)
           AND ($10::text IS NULL OR c.external_check_state = $10)
           AND (NOT $11 OR c.owned_by_batch_change_id = $1)
           AND (NOT $12 OR (c.owned_by_batch_change_id = $1 AND c.computed_state IN ('OPEN', 'DRAFT')))
           AND ($1::bigint IS NULL OR $13 = 'include'
                OR ($13 = 'only') = ({ARCHIVED_SQL}))
           AND NOT EXISTS (SELECT 1 FROM unnest($14::text[]) t
                           WHERE NOT (COALESCE(c.title, s.title, '') ILIKE t OR r.name ILIKE t
                                      OR COALESCE(c.external_id, '') ILIKE t))
           AND NOT EXISTS (SELECT 1 FROM unnest($15::text[]) t
                           WHERE COALESCE(c.title, s.title, '') ILIKE t OR r.name ILIKE t
                                 OR COALESCE(c.external_id, '') ILIKE t)
           AND ($16::bigint IS NULL OR c.id > $16)"
    )
}

struct ListBinds {
    readable: Option<Vec<i32>>,
    states: Vec<&'static str>,
    reconciler: Vec<&'static str>,
    external: Vec<&'static str>,
    archived: &'static str,
    pos: Vec<String>,
    neg: Vec<String>,
}

impl ListBinds {
    fn new(opts: &ListChangesets, access: &RepoAccess) -> Self {
        let (pos, neg) = opts.search.as_deref().map(search_terms).unwrap_or_default();
        Self {
            readable: access.readable_ids(),
            states: opts.states.iter().map(|s| s.as_str()).collect(),
            reconciler: opts.reconciler_states.iter().map(|s| s.as_str()).collect(),
            external: opts.external_states.iter().map(|s| s.as_str()).collect(),
            archived: match opts.archived {
                ArchivedFilter::Exclude => "exclude",
                ArchivedFilter::Only => "only",
                ArchivedFilter::Include => "include",
            },
            pos,
            neg,
        }
    }
}

macro_rules! bind_list {
    ($q:expr, $opts:expr, $b:expr, $after:expr) => {
        $q.bind($opts.batch_change_id)
            .bind($opts.repo.map(|r| r.get()))
            .bind(&$b.readable)
            .bind(&$opts.ids)
            .bind(&$b.states)
            .bind(&$b.reconciler)
            .bind($opts.publication_state.map(|s| s.as_str()))
            .bind(&$b.external)
            .bind($opts.review_state.map(|s| s.as_str()))
            .bind($opts.check_state.map(|s| s.as_str()))
            .bind($opts.owned_by_batch_change)
            .bind($opts.only_closable)
            .bind($b.archived)
            .bind(&$b.pos)
            .bind(&$b.neg)
            .bind($after)
    };
}

/// Store for changesets and their events.
#[derive(Debug, Clone)]
pub struct ChangesetStore {
    pool: PgPool,
}

impl ChangesetStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The reconciler queue.
    pub fn queue_config() -> QueueConfig {
        // 60 retries and resets: publishing a large bulk change outpaces
        // the code host (a busy GitLab refuses merge requests for branches it has not
        // processed yet, rate limits), and transient errors must not fail changesets for
        // good. Retries back off from 5 s to at most 5 minutes.
        QueueConfig {
            max_failures: 60,
            max_resets: 60,
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(300),
            ..QueueConfig::new(CHANGESETS_TABLE)
        }
    }

    /// The changesets a preview of a bulk change spec maps its changeset specs to (see
    /// [`ApplyTx::mapping_candidates`](super::ApplyTx::mapping_candidates)).
    pub async fn mapping_candidates(
        &self,
        batch_change_id: Option<i64>,
        imports: &[(RepoId, String)],
    ) -> Result<Vec<Changeset>> {
        let mut conn = self.pool.acquire().await?;
        mapping_candidates(&mut conn, batch_change_id, imports, false).await
    }

    /// Creates a changeset and wakes the reconciler.
    pub async fn create(&self, new: &NewChangeset) -> Result<Changeset> {
        let mut tx = self.pool.begin().await?;
        let c = create(&mut tx, new).await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Looks up a changeset.
    pub async fn get(&self, id: i64) -> Result<Option<Changeset>> {
        Ok(sqlx::query_as("SELECT * FROM changesets WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Looks up changesets (missing ones are left out), in id order.
    pub async fn get_many(&self, ids: &[i64]) -> Result<Vec<Changeset>> {
        Ok(
            sqlx::query_as("SELECT * FROM changesets WHERE id = ANY($1) ORDER BY id")
                .bind(ids)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Looks up the changeset of a merge request (for webhooks and imports).
    pub async fn get_by_external_id(
        &self,
        repo: RepoId,
        external_service_type: &str,
        external_id: &str,
    ) -> Result<Option<Changeset>> {
        Ok(sqlx::query_as(
            "SELECT * FROM changesets
             WHERE repo_id = $1 AND external_service_type = $2 AND external_id = $3",
        )
        .bind(repo.get())
        .bind(external_service_type)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Published changesets of `repo` whose head is `external_branch` (pipeline webhooks
    /// that name no merge request), in id order.
    pub async fn by_branch(
        &self,
        repo: RepoId,
        external_service_type: &str,
        external_branch: &str,
    ) -> Result<Vec<Changeset>> {
        Ok(sqlx::query_as(
            "SELECT * FROM changesets
             WHERE repo_id = $1 AND external_service_type = $2 AND external_branch = $3
               AND external_id IS NOT NULL
             ORDER BY id",
        )
        .bind(repo.get())
        .bind(external_service_type)
        .bind(external_branch)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Another changeset than `id` with branch `external_branch` in `repo` (the reconciler
    /// refuses to push a second changeset to the same branch).
    pub async fn other_on_branch(
        &self,
        id: i64,
        repo: RepoId,
        external_service_type: &str,
        external_branch: &str,
    ) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM changesets
             WHERE repo_id = $1 AND external_service_type = $2 AND external_branch = $3
               AND id <> $4
             ORDER BY id LIMIT 1",
        )
        .bind(repo.get())
        .bind(external_service_type)
        .bind(external_branch)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Changesets matching `opts` in repositories `access` may read, in id order.
    pub async fn list(&self, opts: &ListChangesets, access: &RepoAccess) -> Result<Vec<Changeset>> {
        let b = ListBinds::new(opts, access);
        let sql = format!(
            "SELECT c.* FROM {} ORDER BY c.id LIMIT CASE WHEN $17::bigint > 0 THEN $17::bigint END",
            list_from()
        );
        Ok(bind_list!(sqlx::query_as(&sql), opts, b, opts.after_id)
            .bind(opts.limit)
            .fetch_all(&self.pool)
            .await?)
    }

    /// The number of changesets matching `opts` (ignoring `after_id` and `limit`).
    pub async fn count(&self, opts: &ListChangesets, access: &RepoAccess) -> Result<i64> {
        let b = ListBinds::new(opts, access);
        let sql = format!("SELECT count(*) FROM {}", list_from());
        Ok(bind_list!(sqlx::query_scalar(&sql), opts, b, None::<i64>)
            .fetch_one(&self.pool)
            .await?)
    }

    /// The ids of all changesets matching `opts` (for bulk operations over a filter).
    pub async fn matching_ids(
        &self,
        opts: &ListChangesets,
        access: &RepoAccess,
    ) -> Result<Vec<i64>> {
        let b = ListBinds::new(opts, access);
        let sql = format!("SELECT c.id FROM {} ORDER BY c.id", list_from());
        Ok(bind_list!(sqlx::query_scalar(&sql), opts, b, opts.after_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Counts of a bulk change's changesets by UI state, in repositories `access` may read.
    pub async fn counts(
        &self,
        batch_change_id: i64,
        access: &RepoAccess,
    ) -> Result<ChangesetCounts> {
        let rows: Vec<(String, bool, i64)> = sqlx::query_as(&format!(
            "SELECT c.computed_state, {ARCHIVED_SQL} AS archived, count(*)
             FROM changesets c JOIN repo r ON r.id = c.repo_id AND r.deleted_at IS NULL
             WHERE c.batch_change_ids ? ($1::bigint)::text
               AND ($2::int[] IS NULL OR c.repo_id = ANY($2))
             GROUP BY 1, 2"
        ))
        .bind(batch_change_id)
        .bind(access.readable_ids())
        .fetch_all(&self.pool)
        .await?;
        let mut counts = ChangesetCounts::default();
        for (state, archived, n) in rows {
            if archived {
                counts.archived += n;
            } else if let Some(s) = ChangesetState::parse(&state) {
                counts.add(s, n);
            }
        }
        Ok(counts)
    }

    /// Re-queues changesets for the reconciler (resetting failures) and wakes it. Returns
    /// how many were queued.
    pub async fn enqueue(&self, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = enqueue(&mut tx, ids, false).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Like [`enqueue`](Self::enqueue), but `scheduled`: they wait for a rollout window.
    pub async fn schedule(&self, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = enqueue(&mut tx, ids, true).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Scheduled changesets in the order they will be released (oldest update first).
    pub async fn scheduled_ids(&self, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT id FROM changesets WHERE state = 'scheduled' ORDER BY updated_at, id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// How many scheduled changesets are released before `id` (0: it is next); `None` when
    /// it is not scheduled.
    pub async fn place_in_schedule(&self, id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM changesets o
                     WHERE o.state = 'scheduled' AND (o.updated_at, o.id) < (c.updated_at, c.id))
             FROM changesets c WHERE c.id = $1 AND c.state = 'scheduled'",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Moves up to `n` scheduled changesets (oldest update first) to `queued`: a rollout
    /// window granted them. Wakes the reconciler and returns their ids. Releases nothing
    /// ([`sourcegull_db::DbError::Fenced`]) when `fence` is no longer current.
    pub async fn release_scheduled(&self, n: i64, fence: &Fence) -> Result<Vec<i64>> {
        let mut tx = fence.begin(&self.pool).await?;
        let ids: Vec<i64> = sqlx::query_scalar(&format!(
            "{RELEASE_SQL} (SELECT id FROM changesets WHERE state = 'scheduled'
                 ORDER BY updated_at, id FOR UPDATE SKIP LOCKED LIMIT $1) RETURNING id"
        ))
        .bind(n)
        .fetch_all(&mut *tx)
        .await?;
        release_done(tx, &ids).await?;
        Ok(ids)
    }

    /// Releases the given changesets from `scheduled` at once, regardless of rollout windows
    /// (publishing them now). Others are left alone. Returns the released ids.
    pub async fn release_now(&self, ids: &[i64]) -> Result<Vec<i64>> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<i64> = sqlx::query_scalar(&format!(
            "{RELEASE_SQL} (SELECT id FROM changesets WHERE state = 'scheduled' AND id = ANY($1)
                 ORDER BY id FOR UPDATE) RETURNING id"
        ))
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        release_done(tx, &ids).await?;
        Ok(ids)
    }

    /// Stores what a webhook said about a changeset. Returns whether it exists.
    pub async fn update_from_hook(&self, id: i64, u: &ChangesetHookUpdate) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE changesets c SET
                 external_state = COALESCE($2, external_state),
                 external_review_state = COALESCE($3, external_review_state),
                 external_check_state = COALESCE($4, external_check_state),
                 external_updated_at = GREATEST(external_updated_at, $5),
                 title = COALESCE($6, title),
                 body = COALESCE($7, body),
                 metadata = c.metadata || (
                     SELECT COALESCE(jsonb_object_agg(p.key,
                         CASE WHEN jsonb_typeof(c.metadata -> p.key) = 'object'
                                   AND jsonb_typeof(p.value) = 'object'
                              THEN (c.metadata -> p.key) || p.value
                              ELSE p.value END), '{}'::jsonb)
                     FROM jsonb_each($8) p),
                 next_sync_at = COALESCE($9, next_sync_at),
                 updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(u.external_state.map(|v| v.as_str()))
        .bind(u.external_review_state.map(|v| v.as_str()))
        .bind(u.external_check_state.map(|v| v.as_str()))
        .bind(u.external_updated_at)
        .bind(&u.title)
        .bind(&u.body)
        .bind(Json(&u.metadata))
        .bind(u.next_sync_at)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Deletes a changeset's events whose key starts with `prefix` (webhook events a full
    /// sync replaced).
    pub async fn delete_events_with_prefix(&self, changeset_id: i64, prefix: &str) -> Result<u64> {
        delete_events_with_prefix(&mut *self.pool.acquire().await?, changeset_id, prefix).await
    }

    /// Stores a sync of changeset `id` in one transaction: [`Self::update_from_sync`], then,
    /// with `events`, [`Self::upsert_events`] and [`Self::delete_events_with_prefix`] of
    /// `replaced_prefix`. Writes nothing ([`sourcegull_db::DbError::Fenced`]) when `fence` is no
    /// longer current.
    pub async fn store_sync(
        &self,
        id: i64,
        s: &ChangesetSync,
        events: Option<&[NewChangesetEvent]>,
        replaced_prefix: &str,
        fence: &Fence,
    ) -> Result<()> {
        let mut tx = fence.begin(&self.pool).await?;
        update_from_sync(&mut tx, id, s).await?;
        if let Some(events) = events {
            upsert_events(&mut tx, id, events).await?;
            delete_events_with_prefix(&mut tx, id, replaced_prefix).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stores what the reconciler or syncer read from the code host, clearing the syncer
    /// error. With a publication state, also clears `closing`.
    pub async fn update_from_sync(&self, id: i64, s: &ChangesetSync) -> Result<bool> {
        update_from_sync(&mut *self.pool.acquire().await?, id, s).await
    }

    /// Records a failed sync and when to try again, while `fence` is current.
    pub async fn set_syncer_error(
        &self,
        id: i64,
        error: &str,
        next_sync_at: Option<DateTime<Utc>>,
        fence: &Fence,
    ) -> Result<bool> {
        let mut tx = fence.begin(&self.pool).await?;
        let n = sqlx::query(
            "UPDATE changesets SET syncer_error = $2, next_sync_at = $3, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(error)
        .bind(next_sync_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Published changesets due for a sync (never synced first), up to `limit`.
    pub async fn due_for_sync(&self, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT c.id FROM changesets c
             JOIN repo r ON r.id = c.repo_id AND r.deleted_at IS NULL
             WHERE c.external_id IS NOT NULL AND c.external_deleted_at IS NULL
               AND (c.next_sync_at IS NULL OR c.next_sync_at <= now())
             ORDER BY c.next_sync_at NULLS FIRST, c.id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Marks changesets to be closed on the code host by the reconciler and queues them.
    pub async fn close(&self, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE changesets SET closing = true, updated_at = now() WHERE id = ANY($1)")
            .bind(ids)
            .execute(&mut *tx)
            .await?;
        let n = enqueue(&mut tx, ids, false).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Requests detaching changesets from a bulk change (done by the reconciler) and
    /// queues them.
    pub async fn request_detach(&self, batch_change_id: i64, ids: &[i64]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let n = set_assoc(&mut tx, batch_change_id, ids, AssocChange::Detach).await?;
        enqueue(&mut tx, ids, false).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Carries out a detach (or a detach of a changeset no bulk change owns): removes the
    /// bulk change from the changeset and stamps `detached_at` when none is left.
    pub async fn remove_from_batch_change(&self, id: i64, batch_change_id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let n = set_assoc(&mut tx, batch_change_id, &[id], AssocChange::Remove).await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Carries out an archive request: the changeset is archived in the bulk change.
    pub async fn mark_archived(&self, id: i64, batch_change_id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let n = set_assoc(&mut tx, batch_change_id, &[id], AssocChange::Archived).await?;
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Changesets no bulk change uses any more since before `before`
    /// (`batchChanges.changesetsRetention`), for the purge: archived in every bulk change
    /// tracking them (archived before `before`; entries archived before archive times were
    /// recorded count from the changeset's last update), or detached from all of them before
    /// `before`. Changesets no bulk change ever detached (`detached_at` unset) are kept.
    pub async fn purgeable_before(&self, before: DateTime<Utc>, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar(
            "SELECT c.id FROM changesets c
             WHERE CASE WHEN c.batch_change_ids = '{}'::jsonb THEN c.detached_at < $1
                   ELSE NOT EXISTS (
                       SELECT 1 FROM jsonb_each(c.batch_change_ids) e
                       WHERE NOT COALESCE((e.value ->> 'isArchived')::boolean, false)
                          OR COALESCE((e.value ->> 'archivedAt')::timestamptz, c.updated_at)
                             >= $1)
                   END
             ORDER BY c.id LIMIT $2",
        )
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Asks the reconciler to publish a changeset (as a draft with
    /// [`UiPublicationState::Draft`]) by setting its UI publication state, and queues it.
    pub async fn publish_from_ui(&self, id: i64, state: UiPublicationState) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let found = set_ui_publication_state(&mut tx, id, Some(state)).await?;
        if found {
            enqueue(&mut tx, &[id], false).await?;
        }
        tx.commit().await?;
        Ok(found)
    }

    /// Deletes changesets (and their events and jobs).
    pub async fn delete(&self, ids: &[i64]) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM changesets WHERE id = ANY($1)")
            .bind(ids)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Stores events of a changeset, updating ones already stored with the same kind and key.
    pub async fn upsert_events(
        &self,
        changeset_id: i64,
        events: &[NewChangesetEvent],
    ) -> Result<()> {
        upsert_events(&mut *self.pool.acquire().await?, changeset_id, events).await
    }

    /// Events of the given changesets (all kinds when `kinds` is empty), oldest first.
    pub async fn events(
        &self,
        changeset_ids: &[i64],
        kinds: &[String],
    ) -> Result<Vec<ChangesetEvent>> {
        Ok(sqlx::query_as(
            "SELECT id, changeset_id, kind, key, metadata, created_at, updated_at
             FROM changeset_events
             WHERE changeset_id = ANY($1) AND (cardinality($2::text[]) = 0 OR kind = ANY($2))
             ORDER BY created_at, id",
        )
        .bind(changeset_ids)
        .bind(kinds)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// Moves scheduled changesets (`WHERE id IN …` follows) to `queued`, released by a window.
const RELEASE_SQL: &str =
    "UPDATE changesets SET state = 'queued', window_released = true, updated_at = now()
     WHERE id IN";

async fn update_from_sync(conn: &mut PgConnection, id: i64, s: &ChangesetSync) -> Result<bool> {
    let metadata = if s.metadata.is_object() {
        s.metadata.clone()
    } else {
        serde_json::json!({})
    };
    Ok(sqlx::query(
        "UPDATE changesets SET
             external_id = COALESCE($2, external_id),
             external_branch = COALESCE($3, external_branch),
             external_fork_namespace = $4, external_fork_name = $5,
             external_state = $6, external_review_state = $7, external_check_state = $8,
             external_updated_at = $9, external_deleted_at = $10,
             diff_stat_added = $11, diff_stat_deleted = $12, metadata = $13,
             title = $14, body = $15, sync_state = $16,
             publication_state = COALESCE($17, publication_state),
             closing = CASE WHEN $17::text IS NULL THEN closing ELSE false END,
             next_sync_at = $18, last_synced_at = now(), syncer_error = NULL,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&s.external_id)
    .bind(&s.external_branch)
    .bind(&s.external_fork_namespace)
    .bind(&s.external_fork_name)
    .bind(s.external_state.map(|v| v.as_str()))
    .bind(s.external_review_state.map(|v| v.as_str()))
    .bind(s.external_check_state.map(|v| v.as_str()))
    .bind(s.external_updated_at)
    .bind(s.external_deleted_at)
    .bind(s.diff_stat_added)
    .bind(s.diff_stat_deleted)
    .bind(Json(metadata))
    .bind(&s.title)
    .bind(&s.body)
    .bind(Json(&s.sync_state))
    .bind(s.publication_state.map(|v| v.as_str()))
    .bind(s.next_sync_at)
    .execute(conn)
    .await?
    .rows_affected()
        > 0)
}

async fn upsert_events(
    conn: &mut PgConnection,
    changeset_id: i64,
    events: &[NewChangesetEvent],
) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    let keys: Vec<&str> = events.iter().map(|e| e.key.as_str()).collect();
    let created: Vec<DateTime<Utc>> = events.iter().map(|e| e.created_at).collect();
    let metadata: Vec<Json<&serde_json::Value>> =
        events.iter().map(|e| Json(&e.metadata)).collect();
    sqlx::query(
        "INSERT INTO changeset_events (changeset_id, kind, key, created_at, metadata)
         SELECT $1, * FROM unnest($2::text[], $3::text[], $4::timestamptz[], $5::jsonb[])
         ON CONFLICT (changeset_id, kind, key) DO UPDATE SET
             metadata = EXCLUDED.metadata, updated_at = now()",
    )
    .bind(changeset_id)
    .bind(&kinds)
    .bind(&keys)
    .bind(&created)
    .bind(&metadata)
    .execute(conn)
    .await?;
    Ok(())
}

async fn delete_events_with_prefix(
    conn: &mut PgConnection,
    changeset_id: i64,
    prefix: &str,
) -> Result<u64> {
    Ok(
        sqlx::query(
            "DELETE FROM changeset_events WHERE changeset_id = $1 AND starts_with(key, $2)",
        )
        .bind(changeset_id)
        .bind(prefix)
        .execute(conn)
        .await?
        .rows_affected(),
    )
}

async fn release_done(mut tx: sqlx::Transaction<'_, sqlx::Postgres>, ids: &[i64]) -> Result<()> {
    if !ids.is_empty() {
        notify_enqueued(&mut *tx, CHANGESETS_TABLE).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn create(conn: &mut PgConnection, new: &NewChangeset) -> Result<Changeset> {
    let ids: BTreeMap<String, BatchChangeAssoc> = new
        .batch_change_id
        .map(|id| (id.to_string(), BatchChangeAssoc::default()))
        .into_iter()
        .collect();
    let c: Changeset = sqlx::query_as(
        "INSERT INTO changesets (repo_id, external_service_type, external_id, external_branch,
             external_fork_namespace, external_fork_name, batch_change_ids,
             owned_by_batch_change_id, current_spec_id, publication_state, ui_publication_state,
             state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                 CASE WHEN $12 THEN 'scheduled' ELSE 'queued' END)
         RETURNING *",
    )
    .bind(new.repo_id.get())
    .bind(&new.external_service_type)
    .bind(&new.external_id)
    .bind(&new.external_branch)
    .bind(&new.external_fork_namespace)
    .bind(&new.external_fork_name)
    .bind(Json(ids))
    .bind(new.owned_by_batch_change_id)
    .bind(new.current_spec_id)
    .bind(new.publication_state.as_str())
    .bind(new.ui_publication_state.map(|s| s.as_str()))
    .bind(new.scheduled)
    .fetch_one(&mut *conn)
    .await?;
    if !new.scheduled {
        notify_enqueued(&mut *conn, CHANGESETS_TABLE).await?;
    }
    Ok(c)
}

/// Resets the queue columns of changesets to `queued` (or `scheduled`), whatever their
/// state: a running reconciliation loses the row and the next one sees the newest spec.
pub(super) async fn enqueue(conn: &mut PgConnection, ids: &[i64], scheduled: bool) -> Result<u64> {
    let n = sqlx::query(
        "UPDATE changesets SET state = CASE WHEN $2 THEN 'scheduled' ELSE 'queued' END,
             num_resets = 0, num_failures = 0, queued_at = now(), started_at = NULL,
             window_released = false,
             finished_at = NULL, process_after = NULL, worker_hostname = '',
             last_heartbeat_at = NULL, failure_message = NULL, cancel = false,
             updated_at = now()
         WHERE id = ANY($1)",
    )
    .bind(ids)
    .bind(scheduled)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n > 0 && !scheduled {
        notify_enqueued(&mut *conn, CHANGESETS_TABLE).await?;
    }
    Ok(n)
}

/// A change to the bulk change entries of changesets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AssocChange {
    /// Track the changeset with no pending request (attach or re-attach, un-archiving).
    Attach,
    /// Request a detach.
    Detach,
    /// Request archiving.
    Archive,
    /// The archive was carried out.
    Archived,
    /// The detach was carried out: drop the entry.
    Remove,
}

pub(super) async fn set_assoc(
    conn: &mut PgConnection,
    batch_change_id: i64,
    ids: &[i64],
    change: AssocChange,
) -> Result<u64> {
    let key = batch_change_id.to_string();
    let sql = match change {
        AssocChange::Attach => {
            "UPDATE changesets SET detached_at = NULL, updated_at = now(),
                 batch_change_ids = batch_change_ids || jsonb_build_object($1::text,
                     '{\"detach\": false, \"archive\": false, \"isArchived\": false}'::jsonb)
             WHERE id = ANY($2)"
        }
        AssocChange::Detach => {
            "UPDATE changesets SET updated_at = now(),
                 batch_change_ids = jsonb_set(batch_change_ids, ARRAY[$1::text, 'detach'], 'true')
             WHERE id = ANY($2) AND batch_change_ids ? $1::text"
        }
        AssocChange::Archive => {
            "UPDATE changesets SET updated_at = now(),
                 batch_change_ids = jsonb_set(batch_change_ids, ARRAY[$1::text, 'archive'], 'true')
             WHERE id = ANY($2) AND batch_change_ids ? $1::text"
        }
        AssocChange::Archived => {
            // `archivedAt` is when the retention (`batchChanges.changesetsRetention`) starts.
            "UPDATE changesets SET updated_at = now(),
                 batch_change_ids = jsonb_set(batch_change_ids, ARRAY[$1::text],
                     jsonb_build_object('detach', false, 'archive', false, 'isArchived', true,
                                        'archivedAt', now()))
             WHERE id = ANY($2) AND batch_change_ids ? $1::text"
        }
        AssocChange::Remove => {
            "UPDATE changesets SET updated_at = now(),
                 batch_change_ids = batch_change_ids - $1::text,
                 detached_at = CASE WHEN (batch_change_ids - $1::text) = '{}'::jsonb
                                    THEN now() ELSE detached_at END
             WHERE id = ANY($2) AND batch_change_ids ? $1::text"
        }
    };
    Ok(sqlx::query(sql)
        .bind(&key)
        .bind(ids)
        .execute(&mut *conn)
        .await?
        .rows_affected())
}

/// Points a changeset at a new spec and cancels a pending close. The old spec becomes
/// `previous_spec_id` only if the reconciler completed it; otherwise the previous spec stays
/// what the code host last saw, so the next plan covers both changes.
pub(super) async fn set_spec(conn: &mut PgConnection, id: i64, spec_id: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE changesets SET current_spec_id = $2, closing = false, updated_at = now(),
             previous_spec_id = CASE WHEN state = 'completed' THEN current_spec_id
                                     ELSE previous_spec_id END
         WHERE id = $1",
    )
    .bind(id)
    .bind(spec_id)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0)
}

pub(super) async fn set_ui_publication_state(
    conn: &mut PgConnection,
    id: i64,
    state: Option<UiPublicationState>,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE changesets SET ui_publication_state = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(state.map(|s| s.as_str()))
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0)
}

pub(super) async fn of_batch_change_for_update(
    conn: &mut PgConnection,
    batch_change_id: i64,
) -> Result<Vec<Changeset>> {
    Ok(sqlx::query_as(
        "SELECT * FROM changesets WHERE batch_change_ids ? ($1::bigint)::text ORDER BY id FOR UPDATE",
    )
    .bind(batch_change_id)
    .fetch_all(&mut *conn)
    .await?)
}

/// The changesets an apply of a bulk change spec maps its changeset specs to: those tracked or
/// created by `batch_change_id` (if any) and those of the `imports` (repository and external
/// id pairs), in id order. Locked with `lock`.
pub(super) async fn mapping_candidates(
    conn: &mut PgConnection,
    batch_change_id: Option<i64>,
    imports: &[(RepoId, String)],
    lock: bool,
) -> Result<Vec<Changeset>> {
    let repos: Vec<i32> = imports.iter().map(|(r, _)| r.get()).collect();
    let ext: Vec<&str> = imports.iter().map(|(_, e)| e.as_str()).collect();
    Ok(sqlx::query_as(&format!(
        "SELECT * FROM changesets c
         WHERE ($1::bigint IS NOT NULL
                AND (c.batch_change_ids ? ($1::bigint)::text OR c.owned_by_batch_change_id = $1))
            OR (c.repo_id, c.external_id) IN (SELECT * FROM unnest($2::int[], $3::text[]))
         ORDER BY c.id{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind(batch_change_id)
    .bind(&repos)
    .bind(&ext)
    .fetch_all(&mut *conn)
    .await?)
}
