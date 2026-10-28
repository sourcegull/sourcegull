//! `batch_specs` and `batch_spec_resolution_jobs`.
//!
//! A bulk change spec is created by a local run (`createBatchSpec`, with changeset specs it
//! uploaded) or from raw YAML for server-side execution (`createBatchSpecFromRaw`). A
//! server-side spec's workspaces are resolved by a worker through the resolution queue: one
//! [`ResolutionJob`] per spec, re-queued when the spec is resolved again.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sourcegull_pgqueue::{QueueConfig, Record, notify_enqueued};
use sourcegull_types::{OrgId, UserId};
use sqlx::PgPool;
use sqlx::types::Json;

use sourcegull_db::{Db, DbError, Namespace, Result};

/// How long a bulk change spec that is never applied is kept (7 days).
pub const BATCH_SPEC_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The queue table of workspace resolution jobs.
pub const BATCH_SPEC_RESOLUTION_JOBS_TABLE: &str = "batch_spec_resolution_jobs";

/// A bulk change spec.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchSpec {
    pub id: i64,
    /// The id used in GraphQL ids.
    pub rand_id: String,
    /// The spec as uploaded (YAML or JSON).
    pub raw_spec: String,
    /// The parsed spec.
    pub spec: serde_json::Value,
    pub namespace: Namespace,
    /// `None` once the creator has been deleted.
    pub user_id: Option<UserId>,
    pub created_from_raw: bool,
    pub allow_unsupported: bool,
    pub allow_ignored: bool,
    pub no_cache: bool,
    /// The bulk change a server-side spec was created for.
    pub batch_change_id: Option<i64>,
    /// When `executeBatchSpec` ran (server-side specs).
    pub execution_requested_at: Option<DateTime<Utc>>,
    /// Apply the spec once its executions completed.
    pub auto_apply_enabled: bool,
    /// When the spec was applied automatically.
    pub auto_applied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A bulk change spec to create.
#[derive(Debug, Clone, PartialEq)]
pub struct NewBatchSpec {
    pub raw_spec: String,
    pub spec: serde_json::Value,
    pub namespace: Namespace,
    pub user_id: Option<UserId>,
    pub created_from_raw: bool,
    pub allow_unsupported: bool,
    pub allow_ignored: bool,
    pub no_cache: bool,
    pub batch_change_id: Option<i64>,
}

/// Filters of [`BatchSpecStore::list`]. Specs are listed newest first.
#[derive(Debug, Clone, Default)]
pub struct ListBatchSpecs {
    pub namespace: Option<Namespace>,
    pub batch_change_id: Option<i64>,
    /// Only specs created by this user.
    pub user_id: Option<UserId>,
    /// Include specs uploaded by local runs (otherwise only server-side ones).
    pub include_local: bool,
    /// Leave out empty specs (no `on` and no `importChangesets`, e.g. the spec of a batch
    /// change created empty).
    pub exclude_empty: bool,
    /// Only specs with an id below this cursor (the last id of the previous page).
    pub before_id: Option<i64>,
    /// Page size; no limit when 0.
    pub limit: i64,
}

#[derive(sqlx::FromRow)]
struct SpecRow {
    id: i64,
    rand_id: String,
    raw_spec: String,
    spec: Json<serde_json::Value>,
    namespace_user_id: Option<UserId>,
    namespace_org_id: Option<OrgId>,
    user_id: Option<UserId>,
    created_from_raw: bool,
    allow_unsupported: bool,
    allow_ignored: bool,
    no_cache: bool,
    batch_change_id: Option<i64>,
    execution_requested_at: Option<DateTime<Utc>>,
    auto_apply_enabled: bool,
    auto_applied_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<SpecRow> for BatchSpec {
    type Error = DbError;

    fn try_from(r: SpecRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            rand_id: r.rand_id,
            raw_spec: r.raw_spec,
            spec: r.spec.0,
            namespace: Namespace::from_columns(r.namespace_user_id, r.namespace_org_id)
                .ok_or(DbError::Invalid("bulk change spec without namespace"))?,
            user_id: r.user_id,
            created_from_raw: r.created_from_raw,
            allow_unsupported: r.allow_unsupported,
            allow_ignored: r.allow_ignored,
            no_cache: r.no_cache,
            batch_change_id: r.batch_change_id,
            execution_requested_at: r.execution_requested_at,
            auto_apply_enabled: r.auto_apply_enabled,
            auto_applied_at: r.auto_applied_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// The payload of a workspace resolution job.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ResolutionJob {
    pub batch_spec_id: i64,
    /// The user the resolution runs as (repository permissions, search).
    pub initiator_id: UserId,
}

/// A resolution job with its queue state.
pub type ResolutionJobRow = Record<ResolutionJob>;

/// `WHERE` clause of [`ListBatchSpecs`] over `batch_specs s`, binds `$1`..`$7`.
const LIST_FILTER: &str = "($1::bigint IS NULL OR s.namespace_user_id = $1)
    AND ($2::bigint IS NULL OR s.namespace_org_id = $2)
    AND ($3::bigint IS NULL OR s.batch_change_id = $3)
    AND ($4::bigint IS NULL OR s.user_id = $4)
    AND ($5 OR s.created_from_raw)
    AND ($6::bigint IS NULL OR s.id < $6)
    AND (NOT $7 OR s.spec ?| array['on', 'importChangesets'])";

/// Store for bulk change specs and their resolution jobs.
#[derive(Debug, Clone)]
pub struct BatchSpecStore {
    pool: PgPool,
}

impl BatchSpecStore {
    pub fn new(db: &Db) -> Self {
        Self {
            pool: db.pool().clone(),
        }
    }

    /// The resolution queue: a worker resolves workspaces of one spec per job.
    pub fn resolution_queue_config() -> QueueConfig {
        QueueConfig::new(BATCH_SPEC_RESOLUTION_JOBS_TABLE)
    }

    /// Creates a spec.
    pub async fn create(&self, new: &NewBatchSpec) -> Result<BatchSpec> {
        let (user, org) = Namespace::columns(Some(new.namespace));
        let row: SpecRow = sqlx::query_as(
            "INSERT INTO batch_specs (raw_spec, spec, namespace_user_id, namespace_org_id,
                 user_id, created_from_raw, allow_unsupported, allow_ignored, no_cache,
                 batch_change_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
        )
        .bind(&new.raw_spec)
        .bind(Json(&new.spec))
        .bind(user)
        .bind(org)
        .bind(new.user_id)
        .bind(new.created_from_raw)
        .bind(new.allow_unsupported)
        .bind(new.allow_ignored)
        .bind(new.no_cache)
        .bind(new.batch_change_id)
        .fetch_one(&self.pool)
        .await?;
        row.try_into()
    }

    /// Looks up a spec.
    pub async fn get(&self, id: i64) -> Result<Option<BatchSpec>> {
        let row: Option<SpecRow> = sqlx::query_as("SELECT * FROM batch_specs WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Looks up a spec by its GraphQL id.
    pub async fn get_by_rand_id(&self, rand_id: &str) -> Result<Option<BatchSpec>> {
        let row: Option<SpecRow> = sqlx::query_as("SELECT * FROM batch_specs WHERE rand_id = $1")
            .bind(rand_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// The newest spec of a bulk change created for it (server-side), if any.
    pub async fn latest_for_batch_change(&self, batch_change_id: i64) -> Result<Option<BatchSpec>> {
        let row: Option<SpecRow> = sqlx::query_as(
            "SELECT * FROM batch_specs WHERE batch_change_id = $1 ORDER BY id DESC LIMIT 1",
        )
        .bind(batch_change_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Specs matching `opts`, newest first.
    pub async fn list(&self, opts: &ListBatchSpecs) -> Result<Vec<BatchSpec>> {
        let (user, org) = Namespace::columns(opts.namespace);
        let rows: Vec<SpecRow> = sqlx::query_as(&format!(
            "SELECT s.* FROM batch_specs s WHERE {LIST_FILTER} ORDER BY s.id DESC
             LIMIT CASE WHEN $8::bigint > 0 THEN $8::bigint END"
        ))
        .bind(user)
        .bind(org)
        .bind(opts.batch_change_id)
        .bind(opts.user_id)
        .bind(opts.include_local)
        .bind(opts.before_id)
        .bind(opts.exclude_empty)
        .bind(opts.limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// The number of specs matching `opts` (ignoring `before_id` and `limit`).
    pub async fn count(&self, opts: &ListBatchSpecs) -> Result<i64> {
        let (user, org) = Namespace::columns(opts.namespace);
        Ok(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM batch_specs s WHERE {LIST_FILTER}"
        ))
        .bind(user)
        .bind(org)
        .bind(opts.batch_change_id)
        .bind(opts.user_id)
        .bind(opts.include_local)
        .bind(None::<i64>)
        .bind(opts.exclude_empty)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Replaces a server-side spec's raw and parsed spec and options (`replaceBatchSpecInput`);
    /// the spec is no longer executed. `None` if there is no such spec.
    pub async fn update(
        &self,
        id: i64,
        raw_spec: &str,
        spec: &serde_json::Value,
        allow_unsupported: bool,
        allow_ignored: bool,
        no_cache: bool,
    ) -> Result<Option<BatchSpec>> {
        let row: Option<SpecRow> = sqlx::query_as(
            "UPDATE batch_specs SET raw_spec = $2, spec = $3, allow_unsupported = $4,
                 allow_ignored = $5, no_cache = $6, execution_requested_at = NULL,
                 auto_apply_enabled = false, auto_applied_at = NULL, updated_at = now()
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(raw_spec)
        .bind(Json(spec))
        .bind(allow_unsupported)
        .bind(allow_ignored)
        .bind(no_cache)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Sets the execution options of a spec (`executeBatchSpec` may override `noCache`).
    pub async fn set_options(
        &self,
        id: i64,
        allow_unsupported: bool,
        allow_ignored: bool,
        no_cache: bool,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE batch_specs SET allow_unsupported = $2, allow_ignored = $3, no_cache = $4,
                 updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(allow_unsupported)
        .bind(allow_ignored)
        .bind(no_cache)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Records that a spec is executed (`executeBatchSpec`), with its `noCache` and
    /// `autoApplyEnabled` options. Returns false when the spec was already executed or does
    /// not exist, so concurrent calls execute it once.
    pub async fn mark_execution_requested(
        &self,
        id: i64,
        no_cache: bool,
        auto_apply_enabled: bool,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE batch_specs SET execution_requested_at = now(), no_cache = $2,
                 auto_apply_enabled = $3, auto_applied_at = NULL, updated_at = now()
             WHERE id = $1 AND execution_requested_at IS NULL",
        )
        .bind(id)
        .bind(no_cache)
        .bind(auto_apply_enabled)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Undoes [`mark_execution_requested`](Self::mark_execution_requested) when queueing the
    /// execution failed afterwards.
    pub async fn clear_execution_requested(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE batch_specs SET execution_requested_at = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Claims the automatic apply of a spec with `autoApplyEnabled`: true for the one caller
    /// that should apply it (not applied yet), false otherwise.
    pub async fn claim_auto_apply(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query(
            "UPDATE batch_specs SET auto_applied_at = now(), updated_at = now()
             WHERE id = $1 AND auto_apply_enabled AND auto_applied_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }

    /// Allows the spec to be applied automatically again (after a retry).
    pub async fn reset_auto_apply(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE batch_specs SET auto_applied_at = NULL WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes a spec with its workspaces, executions, resolution job and changeset specs.
    /// Fails while a bulk change has it as its current spec.
    pub async fn delete(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM batch_specs WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Deletes specs older than `ttl` that were never applied (no bulk change points at
    /// them) and do not belong to a bulk change (cleanup of previews).
    pub async fn delete_unapplied_older_than(&self, ttl: Duration) -> Result<u64> {
        Ok(sqlx::query(
            "DELETE FROM batch_specs s
             WHERE s.created_at < now() - make_interval(secs => $1::float8)
               AND s.batch_change_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM batch_changes b WHERE b.batch_spec_id = s.id)",
        )
        .bind(ttl.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Queues the resolution of a spec's workspaces, replacing any earlier job of the spec
    /// (a finished one is reset to `queued`). Returns the job id.
    pub async fn enqueue_resolution(&self, batch_spec_id: i64, initiator: UserId) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query_scalar(
            "INSERT INTO batch_spec_resolution_jobs (batch_spec_id, initiator_id)
             VALUES ($1, $2)
             ON CONFLICT (batch_spec_id) DO UPDATE SET
                 state = 'queued', initiator_id = EXCLUDED.initiator_id, num_resets = 0,
                 num_failures = 0, queued_at = now(), started_at = NULL, finished_at = NULL,
                 process_after = NULL, worker_hostname = '', last_heartbeat_at = NULL,
                 failure_message = NULL, execution_logs = '[]'::jsonb, cancel = false,
                 updated_at = now()
             RETURNING id",
        )
        .bind(batch_spec_id)
        .bind(initiator)
        .fetch_one(&mut *tx)
        .await?;
        notify_enqueued(&mut *tx, BATCH_SPEC_RESOLUTION_JOBS_TABLE).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The resolution job of a spec, if one was queued.
    pub async fn resolution_job(&self, batch_spec_id: i64) -> Result<Option<ResolutionJobRow>> {
        Ok(
            sqlx::query_as("SELECT * FROM batch_spec_resolution_jobs WHERE batch_spec_id = $1")
                .bind(batch_spec_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
}
