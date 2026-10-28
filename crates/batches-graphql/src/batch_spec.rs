//! `BatchSpec` with its workspace resolution and workspaces, changeset specs
//! (`VisibleChangesetSpec`/`HiddenChangesetSpec`), and the mutations that create, replace
//! and delete specs and upload changeset specs.

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql::{Context, Enum, ID, Interface, Object, Result, SimpleObject, Union};
use chrono::{DateTime, Duration, Utc};
use sourcegull_batches::store::{
    BatchChangeStore, BatchSpec, BatchSpecStore, BatchSpecWorkspace, BatchSpecWorkspaceStore,
    ChangesetSpec, ChangesetSpecStore, ChangesetSpecType, ExecutionJobRow, ExecutionStats,
    ListBatchSpecs, ListChangesetSpecs, ListWorkspaces, NewBatchSpec, ResolutionJobRow,
};
use sourcegull_pgqueue::JobState;
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};
use sourcegull_types::{Repo, RepoId};

use super::batch_change::BatchChangeNode;
use super::{
    BATCH_SPEC_KIND, CHANGESET_SPEC_KIND, JsonValue, PublishedValue, WORKSPACE_KIND, batch_spec_id,
    can_administer, changeset_spec_id, check_first, invalid, invalid_id, namespace_node,
    namespace_url, not_found, parse_batch_change_id, parse_cursor, parse_string_id, readable_repo,
    readable_repos, require_namespace_access, require_write, spec_errors, user_node,
};
use crate::context::{codes, db_error, error, require_site_admin, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::diff::DiffStat;
use sourcegull_graphql_core::ids::parse_namespace_id;
use sourcegull_graphql_core::ids::parse_repository_id;
use sourcegull_graphql_core::shared::{
    BranchData, BranchRef, ExecutorRef, NamespaceRef, PersonData, PersonRef, RepositoryRef, UserRef,
};

pub use sourcegull_services::batches::FORK_NAMESPACE_USER;
use sourcegull_services::batches::new_changeset_spec;

/// The state of a bulk change spec.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BatchSpecState")]
pub enum BatchSpecStateGql {
    #[graphql(name = "PENDING")]
    Pending,
    #[graphql(name = "PROCESSING")]
    Processing,
    #[graphql(name = "FAILED")]
    Failed,
    #[graphql(name = "COMPLETED")]
    Completed,
    #[graphql(name = "QUEUED")]
    Queued,
    #[graphql(name = "CANCELING")]
    Canceling,
    #[graphql(name = "CANCELED")]
    Canceled,
}

/// Where a bulk change spec was executed.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BatchSpecSource")]
pub enum BatchSpecSourceGql {
    #[graphql(name = "LOCAL")]
    Local,
    #[graphql(name = "REMOTE")]
    Remote,
}

/// The state of a workspace resolution.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BatchSpecWorkspaceResolutionState")]
pub enum ResolutionStateGql {
    #[graphql(name = "QUEUED")]
    Queued,
    #[graphql(name = "PROCESSING")]
    Processing,
    #[graphql(name = "ERRORED")]
    Errored,
    #[graphql(name = "FAILED")]
    Failed,
    #[graphql(name = "COMPLETED")]
    Completed,
}

/// The state of a workspace's execution.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BatchSpecWorkspaceState")]
pub enum WorkspaceStateGql {
    #[graphql(name = "SKIPPED")]
    Skipped,
    #[graphql(name = "PENDING")]
    Pending,
    #[graphql(name = "QUEUED")]
    Queued,
    #[graphql(name = "PROCESSING")]
    Processing,
    #[graphql(name = "FAILED")]
    Failed,
    #[graphql(name = "COMPLETED")]
    Completed,
    #[graphql(name = "CANCELING")]
    Canceling,
    #[graphql(name = "CANCELED")]
    Canceled,
}

impl WorkspaceStateGql {
    /// The [`ListWorkspaces::state`] filter (`CANCELING` lists running jobs).
    fn filter(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Processing | Self::Canceling => "processing",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }
}

/// The order of a workspace listing.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "WorkspacesSortOrder")]
pub enum WorkspacesSortOrder {
    #[graphql(name = "REPO_NAME_ASC")]
    RepoNameAsc,
    #[graphql(name = "REPO_NAME_DESC")]
    RepoNameDesc,
}

/// A bulk change spec.
#[derive(Clone)]
pub struct BatchSpecNode(pub BatchSpec);

/// The name and description of the bulk change a spec describes.
#[derive(SimpleObject)]
#[graphql(name = "BatchChangeDescription")]
pub struct BatchChangeDescription {
    pub name: String,
    pub description: String,
}

impl BatchSpecNode {
    pub fn new(spec: BatchSpec) -> Self {
        Self(spec)
    }

    fn spec_str(&self, key: &str) -> String {
        self.0
            .spec
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    }

    async fn resolution(&self, ctx: &Context<'_>) -> Result<Option<ResolutionJobRow>> {
        BatchSpecStore::new(&services(ctx)?.db)
            .resolution_job(self.0.id)
            .await
            .map_err(db_error)
    }

    async fn stats(&self, ctx: &Context<'_>) -> Result<ExecutionStats> {
        BatchSpecWorkspaceStore::new(&services(ctx)?.db)
            .stats(self.0.id)
            .await
            .map_err(db_error)
    }

    pub(super) async fn compute_state(&self, ctx: &Context<'_>) -> Result<BatchSpecStateGql> {
        if !self.0.created_from_raw {
            return Ok(BatchSpecStateGql::Completed);
        }
        let resolution = self.resolution(ctx).await?;
        Ok(spec_state(
            resolution.as_ref().map(|r| r.state),
            &self.stats(ctx).await?,
            self.0.execution_requested_at.is_some(),
        ))
    }
}

/// The state of a server-side spec from its resolution job and executions (a failed
/// resolution makes the spec failed). `executed`: the spec
/// was executed, so without execution jobs every workspace was skipped or cached.
fn spec_state(
    resolution: Option<JobState>,
    s: &ExecutionStats,
    executed: bool,
) -> BatchSpecStateGql {
    use BatchSpecStateGql as S;
    match resolution {
        Some(JobState::Completed) => {}
        Some(JobState::Failed | JobState::Canceled) => return S::Failed,
        _ => return S::Pending,
    }
    if s.workspaces == 0 || s.skipped_workspaces == s.workspaces {
        return S::Completed;
    }
    if s.executions == 0 {
        return if executed { S::Completed } else { S::Pending };
    }
    let n = s.executions;
    if s.queued == n {
        S::Queued
    } else if s.completed == n {
        S::Completed
    } else if s.canceled == n {
        S::Canceled
    } else if s.failed + s.completed == n {
        S::Failed
    } else if s.canceled + s.failed + s.completed == n {
        S::Canceled
    } else if s.canceling + s.failed + s.completed + s.canceled == n {
        S::Canceling
    } else {
        S::Processing
    }
}

#[Object(name = "BatchSpec")]
impl BatchSpecNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        batch_spec_id(&self.0.rand_id)
    }

    /// Whether the spec is applied when its execution completes (`executeBatchSpec`).
    async fn auto_apply_enabled(&self) -> bool {
        self.0.auto_apply_enabled
    }

    /// `PENDING` until its workspaces are resolved and executed (server-side specs);
    /// `COMPLETED` for specs uploaded by local runs.
    async fn state(&self, ctx: &Context<'_>) -> Result<BatchSpecStateGql> {
        self.compute_state(ctx).await
    }

    /// The spec as uploaded.
    async fn original_input(&self) -> &str {
        &self.0.raw_spec
    }

    /// The spec parsed to JSON.
    async fn parsed_input(&self) -> JsonValue {
        JsonValue(self.0.spec.clone())
    }

    /// The bulk change's name and description from the spec.
    async fn description(&self) -> BatchChangeDescription {
        BatchChangeDescription {
            name: self.spec_str("name"),
            description: self.spec_str("description"),
        }
    }

    /// The spec's changeset specs, in id order (hidden ones in repositories the viewer may
    /// not read).
    async fn changeset_specs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<ChangesetSpecConnection> {
        let limit = check_first(first)?;
        let store = ChangesetSpecStore::new(&services(ctx)?.db);
        let mut opts = ListChangesetSpecs {
            batch_spec_id: Some(self.0.id),
            ..Default::default()
        };
        let total = store.count(&opts).await.map_err(db_error)?;
        if limit == 0 {
            return Ok(ChangesetSpecConnection {
                total_count: to_int(total),
                page_info: PageInfo {
                    has_next_page: total > 0,
                    end_cursor: None,
                },
                nodes: Vec::new(),
            });
        }
        opts.after_id = parse_cursor(after.as_deref())?;
        opts.limit = limit + 1;
        let mut specs = store.list(&opts).await.map_err(db_error)?;
        let has_next_page = i64::try_from(specs.len()).unwrap_or(i64::MAX) > limit;
        specs.truncate(usize::try_from(limit).unwrap_or(0));
        let end_cursor = has_next_page
            .then(|| specs.last().map(|s| s.id.to_string()))
            .flatten();
        Ok(ChangesetSpecConnection {
            total_count: to_int(total),
            page_info: PageInfo {
                has_next_page,
                end_cursor,
            },
            nodes: changeset_spec_nodes(ctx, specs).await?,
        })
    }

    /// What applying the spec would do to each changeset (see `applyBatchChange`), filtered
    /// by text (`search`, matching title and repository; hidden changesets are left out), the
    /// current state of the changeset, or an operation (`action`). `publicationStates`
    /// previews publishing specs that leave `published` unset.
    #[allow(clippy::too_many_arguments)]
    async fn apply_preview(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        search: Option<String>,
        current_state: Option<super::ChangesetStateGql>,
        action: Option<super::apply::ChangesetSpecOperationGql>,
        publication_states: Option<Vec<super::apply::ChangesetSpecPublicationStateInput>>,
    ) -> Result<super::apply::ChangesetApplyPreviewConnection> {
        super::require_read(ctx)?;
        super::apply::preview(
            ctx,
            &self.0,
            super::apply::PreviewArgs {
                first,
                after,
                search,
                current_state,
                action,
                publication_states,
            },
        )
        .await
    }

    /// The creator; null once deleted.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user_node(ctx, self.0.user_id).await
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the first workspace execution started (server-side specs).
    async fn started_at(&self, ctx: &Context<'_>) -> Result<Option<DateTime<Utc>>> {
        if !self.0.created_from_raw {
            return Ok(None);
        }
        Ok(self.stats(ctx).await?.started_at)
    }

    /// When the last workspace execution finished (server-side specs).
    async fn finished_at(&self, ctx: &Context<'_>) -> Result<Option<DateTime<Utc>>> {
        if !self.0.created_from_raw {
            return Ok(None);
        }
        Ok(self.stats(ctx).await?.finished_at)
    }

    /// The user or organization the spec belongs to.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<NamespaceRef> {
        namespace_node(ctx, self.0.namespace).await
    }

    /// When an unapplied spec is deleted.
    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        Some(self.0.created_at + ttl(sourcegull_batches::store::BATCH_SPEC_TTL))
    }

    /// The page to preview and apply the spec; for server-side specs only once executed.
    #[graphql(name = "applyURL")]
    async fn apply_url(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if self.0.created_from_raw && self.compute_state(ctx).await? != BatchSpecStateGql::Completed
        {
            return Ok(None);
        }
        let id = batch_spec_id(&self.0.rand_id);
        Ok(Some(format!(
            "{}/bulk-changes/apply/{}",
            namespace_url(ctx, self.0.namespace).await?,
            id.0
        )))
    }

    /// Whether the viewer may administer the spec's namespace.
    async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        can_administer(ctx, self.0.namespace).await
    }

    /// The summed diff stat of the changeset specs.
    async fn diff_stat(&self, ctx: &Context<'_>) -> Result<Option<DiffStat>> {
        let (added, deleted) = ChangesetSpecStore::new(&services(ctx)?.db)
            .diff_stat(self.0.id)
            .await
            .map_err(db_error)?;
        Ok(Some(DiffStat {
            added: to_int(added),
            deleted: to_int(deleted),
            changed: 0,
        }))
    }

    /// The bulk change in the spec's namespace with the spec's name, if one exists.
    async fn applies_to_batch_change(&self, ctx: &Context<'_>) -> Result<Option<BatchChangeNode>> {
        let name = self.spec_str("name");
        Ok(BatchChangeStore::new(&services(ctx)?.db)
            .get_by_name(self.0.namespace, &name)
            .await
            .map_err(db_error)?
            .map(BatchChangeNode::new))
    }

    /// The resolution of a server-side spec's workspaces; null for local runs and before it
    /// is queued.
    async fn workspace_resolution(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<WorkspaceResolutionNode>> {
        if !self.0.created_from_raw {
            return Ok(None);
        }
        Ok(self
            .resolution(ctx)
            .await?
            .map(|job| WorkspaceResolutionNode {
                job,
                spec: Arc::new(self.0.clone()),
            }))
    }

    /// The files uploaded with the spec for its steps to `mount` (by a remote run of a command-line
    /// client); null for those who may not administer the spec's namespace.
    async fn files(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
    ) -> Result<Option<super::WorkspaceFileConnection>> {
        if !can_administer(ctx, self.0.namespace).await? {
            return Ok(None);
        }
        super::local::files(ctx, &self.0, first, after)
            .await
            .map(Some)
    }

    /// Why the resolution failed, or the failure messages of failed executions.
    async fn failure_message(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        if let Some(m) = self.resolution(ctx).await?.and_then(|j| j.failure_message) {
            return Ok(Some(m));
        }
        if !self.0.created_from_raw {
            return Ok(None);
        }
        super::execution::failure_message(ctx, &self.0).await
    }

    async fn allow_ignored(&self) -> Option<bool> {
        Some(self.0.allow_ignored)
    }

    async fn allow_unsupported(&self) -> Option<bool> {
        Some(self.0.allow_unsupported)
    }

    async fn no_cache(&self) -> Option<bool> {
        Some(self.0.no_cache)
    }

    /// Whether the viewer may retry the execution: its creator or a site admin, once it
    /// failed or was canceled.
    async fn viewer_can_retry(&self, ctx: &Context<'_>) -> Result<bool> {
        if !self.0.created_from_raw || !super::execution::viewer_can_modify(ctx, &self.0) {
            return Ok(false);
        }
        Ok(matches!(
            self.compute_state(ctx).await?,
            BatchSpecStateGql::Failed | BatchSpecStateGql::Canceled
        ))
    }

    /// `REMOTE` for server-side specs, `LOCAL` for specs uploaded by local runs.
    async fn source(&self) -> BatchSpecSourceGql {
        if self.0.created_from_raw {
            BatchSpecSourceGql::Remote
        } else {
            BatchSpecSourceGql::Local
        }
    }
}

/// A page of bulk change specs.
#[derive(SimpleObject)]
pub struct BatchSpecConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<BatchSpecNode>,
}

/// Lists specs, of a bulk change if given, newest first.
pub(super) async fn list_specs(
    ctx: &Context<'_>,
    batch_change_id: Option<i64>,
    first: Option<i32>,
    after: Option<String>,
    include_local: bool,
    exclude_empty: bool,
) -> Result<BatchSpecConnection> {
    let limit = check_first(first)?;
    let store = BatchSpecStore::new(&services(ctx)?.db);
    let mut opts = ListBatchSpecs {
        batch_change_id,
        include_local,
        exclude_empty,
        before_id: parse_cursor(after.as_deref())?,
        ..Default::default()
    };
    let total = store.count(&opts).await.map_err(db_error)?;
    if limit == 0 {
        return Ok(BatchSpecConnection {
            total_count: to_int(total),
            page_info: PageInfo {
                has_next_page: total > 0,
                end_cursor: None,
            },
            nodes: Vec::new(),
        });
    }
    opts.limit = limit + 1;
    let mut specs = store.list(&opts).await.map_err(db_error)?;
    let has_next_page = i64::try_from(specs.len()).unwrap_or(i64::MAX) > limit;
    specs.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(BatchSpecConnection {
        total_count: to_int(total),
        page_info: PageInfo {
            has_next_page,
            end_cursor: has_next_page
                .then(|| specs.last().map(|s| s.id.to_string()))
                .flatten(),
        },
        nodes: specs.into_iter().map(BatchSpecNode).collect(),
    })
}

/// A spec by ID for `node(id:)`. Anyone who knows a spec's ID may see it (IDs are random),
/// so preview links can be shared.
pub(super) async fn load_spec(ctx: &Context<'_>, id: &str) -> Result<Option<BatchSpecNode>> {
    let Some(rand) = parse_string_id(BATCH_SPEC_KIND, id) else {
        return Ok(None);
    };
    Ok(BatchSpecStore::new(&services(ctx)?.db)
        .get_by_rand_id(&rand)
        .await
        .map_err(db_error)?
        .map(BatchSpecNode))
}

pub(super) async fn spec_by_id(ctx: &Context<'_>, id: &str) -> Result<BatchSpec> {
    let rand = parse_string_id(BATCH_SPEC_KIND, id).ok_or_else(|| invalid_id(id))?;
    BatchSpecStore::new(&services(ctx)?.db)
        .get_by_rand_id(&rand)
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change spec"))
}

/// Server-side spec options.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpecFlags {
    pub allow_ignored: bool,
    pub allow_unsupported: bool,
    pub no_cache: bool,
}

/// Parses and validates a bulk change spec; errors carry positions.
fn parse_spec(raw: &str) -> Result<serde_json::Value> {
    let parsed = sourcegull_batches::parse_batch_spec(raw).map_err(|e| spec_errors(&e))?;
    serde_json::to_value(&parsed.spec)
        .map_err(|_| error(codes::INTERNAL, "The bulk change spec could not be stored."))
}

/// `createBatchSpec`: a local run's spec with the changeset specs it uploaded. Every
/// changeset spec must be the viewer's (or the viewer a site admin), unattached, and in a
/// readable repository.
pub(super) async fn create_batch_spec(
    ctx: &Context<'_>,
    namespace: &str,
    raw: &str,
    changeset_specs: &[ID],
) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = parse_spec(raw)?;
    let ns = parse_namespace_id(namespace)?;
    require_namespace_access(ctx, actor, ns).await?;
    let rand_ids = changeset_specs
        .iter()
        .map(|id| parse_string_id(CHANGESET_SPEC_KIND, id).ok_or_else(|| invalid_id(id)))
        .collect::<Result<Vec<_>>>()?;
    let db = &services(ctx)?.db;
    let found = ChangesetSpecStore::new(db)
        .get_by_rand_ids(&rand_ids)
        .await
        .map_err(db_error)?;
    let repo_ids: Vec<RepoId> = found.iter().map(|s| s.repo_id).collect();
    let readable = readable_repos(ctx, &repo_ids).await?;
    let by_rand: HashMap<&str, &ChangesetSpec> =
        found.iter().map(|s| (s.rand_id.as_str(), s)).collect();
    for rand in &rand_ids {
        // Someone else's spec, or one in an unreadable repository, is "not found" like a
        // missing one.
        let Some(s) = by_rand.get(rand.as_str()).filter(|s| {
            readable.contains_key(&s.repo_id)
                && sourcegull_services::batches::access::is_creator_or_site_admin(actor, s.user_id)
        }) else {
            return Err(not_found(&format!("Changeset spec {rand:?}")));
        };
        if s.batch_spec_id.is_some() {
            return Err(invalid(format!(
                "Changeset spec {rand:?} already belongs to a bulk change spec."
            )));
        }
    }
    let created = BatchSpecStore::new(db)
        .create(&NewBatchSpec {
            raw_spec: raw.to_owned(),
            spec,
            namespace: ns,
            user_id: Some(actor.user_id),
            created_from_raw: false,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .map_err(db_error)?;
    let ids: Vec<i64> = found.iter().map(|s| s.id).collect();
    if !ids.is_empty() {
        let attached = ChangesetSpecStore::new(db)
            .attach_to_batch_spec(created.id, &ids)
            .await
            .map_err(db_error)?;
        if attached != ids.len() as u64 {
            let _ = BatchSpecStore::new(db).delete(created.id).await;
            return Err(invalid(
                "A changeset spec was attached to another bulk change spec concurrently.",
            ));
        }
    }
    Ok(BatchSpecNode(created))
}

/// `createBatchSpecFromRaw`.
pub(super) async fn create_from_raw(
    ctx: &Context<'_>,
    raw: &str,
    flags: SpecFlags,
    namespace: &str,
    batch_change: &str,
) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = parse_spec(raw)?;
    let ns = parse_namespace_id(namespace)?;
    require_namespace_access(ctx, actor, ns).await?;
    let db = &services(ctx)?.db;
    let bc_id = parse_batch_change_id(batch_change)?;
    let bc = BatchChangeStore::new(db)
        .get(bc_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change"))?;
    // The bulk change's namespace may differ from the requested one.
    require_namespace_access(ctx, actor, bc.namespace).await?;
    let created = BatchSpecStore::new(db)
        .create(&NewBatchSpec {
            raw_spec: raw.to_owned(),
            spec,
            namespace: ns,
            user_id: Some(actor.user_id),
            created_from_raw: true,
            allow_unsupported: flags.allow_unsupported,
            allow_ignored: flags.allow_ignored,
            no_cache: flags.no_cache,
            batch_change_id: Some(bc.id),
        })
        .await
        .map_err(db_error)?;
    BatchSpecStore::new(db)
        .enqueue_resolution(created.id, actor.user_id)
        .await
        .map_err(db_error)?;
    Ok(BatchSpecNode(created))
}

/// `replaceBatchSpecInput`: same spec (and ID), new input; earlier workspaces and changeset
/// specs are dropped and the resolution queued again.
pub(super) async fn replace_input(
    ctx: &Context<'_>,
    previous: &str,
    raw: &str,
    flags: SpecFlags,
) -> Result<BatchSpecNode> {
    let actor = require_write(ctx)?;
    let spec = parse_spec(raw)?;
    let old = spec_by_id(ctx, previous).await?;
    require_namespace_access(ctx, actor, old.namespace).await?;
    if !old.created_from_raw {
        return Err(invalid(
            "Only bulk change specs created for server-side execution can be replaced.",
        ));
    }
    let db = &services(ctx)?.db;
    let updated = BatchSpecStore::new(db)
        .update(
            old.id,
            raw,
            &spec,
            flags.allow_unsupported,
            flags.allow_ignored,
            flags.no_cache,
        )
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change spec"))?;
    BatchSpecWorkspaceStore::new(db)
        .delete_for_spec(old.id)
        .await
        .map_err(db_error)?;
    ChangesetSpecStore::new(db)
        .delete_for_batch_spec(old.id)
        .await
        .map_err(db_error)?;
    BatchSpecStore::new(db)
        .enqueue_resolution(old.id, old.user_id.unwrap_or(actor.user_id))
        .await
        .map_err(db_error)?;
    Ok(BatchSpecNode(updated))
}

/// `deleteBatchSpec` (site admins only).
pub(super) async fn delete_spec(ctx: &Context<'_>, id: &str) -> Result<EmptyResponse> {
    require_write(ctx)?;
    require_site_admin(ctx)?;
    let spec = spec_by_id(ctx, id).await?;
    let db = &services(ctx)?.db;
    let in_use: bool = sourcegull_db::sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM batch_changes WHERE batch_spec_id = $1)",
    )
    .bind(spec.id)
    .fetch_one(db.pool())
    .await
    .map_err(|e| db_error(e.into()))?;
    if in_use {
        return Err(invalid(
            "The bulk change spec is the current spec of a bulk change and cannot be deleted.",
        ));
    }
    BatchSpecStore::new(db)
        .delete(spec.id)
        .await
        .map_err(db_error)?;
    Ok(EmptyResponse)
}

// ------------------------------------------------------------------ changeset specs

/// What a changeset spec describes.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "ChangesetSpecType")]
pub enum ChangesetSpecTypeGql {
    #[graphql(name = "EXISTING")]
    Existing,
    #[graphql(name = "BRANCH")]
    Branch,
}

impl From<ChangesetSpecType> for ChangesetSpecTypeGql {
    fn from(t: ChangesetSpecType) -> Self {
        match t {
            ChangesetSpecType::Branch => Self::Branch,
            ChangesetSpecType::Existing => Self::Existing,
        }
    }
}

/// A retention as a [`Duration`] to add to a creation time.
fn ttl(d: std::time::Duration) -> Duration {
    Duration::from_std(d).unwrap_or(Duration::MAX)
}

fn spec_expires_at(s: &ChangesetSpec) -> Option<DateTime<Utc>> {
    s.batch_spec_id
        .is_none()
        .then(|| s.created_at + ttl(sourcegull_batches::store::CHANGESET_SPEC_TTL))
}

/// A changeset spec in a repository the viewer may read.
#[derive(Clone)]
pub struct VisibleChangesetSpecNode {
    spec: Arc<ChangesetSpec>,
    repo: Arc<Repo>,
}

impl VisibleChangesetSpecNode {
    pub fn new(spec: ChangesetSpec, repo: Repo) -> Self {
        Self {
            spec: Arc::new(spec),
            repo: Arc::new(repo),
        }
    }
}

/// A changeset spec in a repository the viewer may not read.
#[derive(Clone)]
pub struct HiddenChangesetSpecNode(Arc<ChangesetSpec>);

impl HiddenChangesetSpecNode {
    pub(super) fn new(spec: ChangesetSpec) -> Self {
        Self(Arc::new(spec))
    }
}

/// A changeset spec (`ChangesetSpec` interface).
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(
    name = "ChangesetSpec",
    field(name = "id", ty = "ID", desc = "The opaque node ID."),
    field(name = "type", method = "spec_type", ty = "ChangesetSpecTypeGql"),
    field(
        name = "expires_at",
        ty = "Option<DateTime<Utc>>",
        desc = "When an unattached spec is deleted."
    )
)]
pub enum ChangesetSpecNode {
    VisibleChangesetSpec(VisibleChangesetSpecNode),
    HiddenChangesetSpec(HiddenChangesetSpecNode),
}

#[Object(name = "HiddenChangesetSpec")]
impl HiddenChangesetSpecNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        changeset_spec_id(&self.0.rand_id)
    }

    #[graphql(name = "type")]
    async fn spec_type(&self) -> ChangesetSpecTypeGql {
        self.0.kind.into()
    }

    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        spec_expires_at(&self.0)
    }
}

/// Where a changeset is pushed when it goes to a fork.
#[derive(SimpleObject)]
pub struct ForkTarget {
    /// Pushed to a fork in the publishing user's namespace.
    pub push_user: bool,
    /// The namespace of the fork, when not the user's.
    pub namespace: Option<String>,
}

/// An existing changeset to import.
pub struct ExistingChangesetReference {
    spec: Arc<ChangesetSpec>,
    repo: Arc<Repo>,
}

#[Object]
impl ExistingChangesetReference {
    async fn base_repository(&self) -> RepositoryRef {
        RepositoryRef((*self.repo).clone())
    }

    #[graphql(name = "externalID")]
    async fn external_id(&self) -> &str {
        self.spec.external_id.as_deref().unwrap_or("")
    }
}

/// A new changeset: a commit on a branch.
pub struct GitBranchChangesetDescription {
    spec: Arc<ChangesetSpec>,
    repo: Arc<Repo>,
}

fn abbreviate_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("refs/tags/"))
        .unwrap_or(r)
}

#[Object]
impl GitBranchChangesetDescription {
    async fn base_repository(&self) -> RepositoryRef {
        RepositoryRef((*self.repo).clone())
    }

    /// The target branch (short name).
    async fn base_ref(&self) -> &str {
        abbreviate_ref(self.spec.base_ref.as_deref().unwrap_or(""))
    }

    /// The commit of the target branch the diff applies to.
    async fn base_rev(&self) -> &str {
        self.spec.base_rev.as_deref().unwrap_or("")
    }

    /// The changeset branch (short name).
    async fn head_ref(&self) -> &str {
        abbreviate_ref(self.spec.head_ref.as_deref().unwrap_or(""))
    }

    async fn title(&self) -> &str {
        self.spec.title.as_deref().unwrap_or("")
    }

    async fn body(&self) -> &str {
        self.spec.body.as_deref().unwrap_or("")
    }

    /// The commit (exactly one).
    async fn commits(&self) -> Vec<GitCommitDescription> {
        vec![GitCommitDescription(self.spec.clone())]
    }

    /// The diff, parsed from the spec (the commit is created when the changeset is
    /// published).
    async fn diff(&self) -> super::preview_diff::PreviewRepositoryComparison {
        super::preview_diff::PreviewRepositoryComparison::new(self.spec.clone(), self.repo.clone())
    }

    /// Lines added and deleted by the diff.
    async fn diff_stat(&self) -> DiffStat {
        DiffStat {
            added: self.spec.diff_stat_added.unwrap_or(0),
            deleted: self.spec.diff_stat_deleted.unwrap_or(0),
            changed: 0,
        }
    }

    /// `true`, `false` or `"draft"`; null when publication is decided in the UI.
    async fn published(&self) -> Option<PublishedValue> {
        self.spec.published.map(PublishedValue)
    }
}

/// The commit of a changeset spec.
pub struct GitCommitDescription(Arc<ChangesetSpec>);

impl GitCommitDescription {
    fn message_str(&self) -> &str {
        self.0.commit_message.as_deref().unwrap_or("")
    }
}

#[Object]
impl GitCommitDescription {
    async fn message(&self) -> &str {
        self.message_str()
    }

    /// The first line of the message.
    async fn subject(&self) -> &str {
        self.message_str().lines().next().unwrap_or("")
    }

    /// The message after the subject and blank line, if any.
    async fn body(&self) -> Option<String> {
        let msg = self.message_str();
        let (_, rest) = msg.split_once('\n')?;
        let body = rest.trim_matches('\n');
        (!body.is_empty()).then(|| body.to_owned())
    }

    async fn author(&self) -> PersonRef {
        let name = self.0.commit_author_name.clone().unwrap_or_default();
        PersonRef(PersonData {
            display_name: name.clone(),
            name,
            email: self.0.commit_author_email.clone().unwrap_or_default(),
            avatar_url: None,
            user: None,
        })
    }

    /// The unified diff.
    async fn diff(&self) -> String {
        String::from_utf8_lossy(self.0.diff.as_deref().unwrap_or_default()).into_owned()
    }
}

/// What a changeset spec describes.
#[derive(Union)]
pub enum ChangesetDescription {
    ExistingChangesetReference(ExistingChangesetReference),
    GitBranchChangesetDescription(GitBranchChangesetDescription),
}

#[Object(name = "VisibleChangesetSpec")]
impl VisibleChangesetSpecNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        changeset_spec_id(&self.spec.rand_id)
    }

    #[graphql(name = "type")]
    async fn spec_type(&self) -> ChangesetSpecTypeGql {
        self.spec.kind.into()
    }

    /// The changeset to create or import.
    async fn description(&self) -> ChangesetDescription {
        let (spec, repo) = (self.spec.clone(), self.repo.clone());
        match self.spec.kind {
            ChangesetSpecType::Existing => {
                ChangesetDescription::ExistingChangesetReference(ExistingChangesetReference {
                    spec,
                    repo,
                })
            }
            ChangesetSpecType::Branch => {
                ChangesetDescription::GitBranchChangesetDescription(GitBranchChangesetDescription {
                    spec,
                    repo,
                })
            }
        }
    }

    async fn expires_at(&self) -> Option<DateTime<Utc>> {
        spec_expires_at(&self.spec)
    }

    /// The fork the changeset is pushed to, if any.
    async fn fork_target(&self) -> Option<ForkTarget> {
        let ns = self.spec.fork_namespace.as_deref()?;
        Some(if ns == FORK_NAMESPACE_USER {
            ForkTarget {
                push_user: true,
                namespace: None,
            }
        } else {
            ForkTarget {
                push_user: false,
                namespace: Some(ns.to_owned()),
            }
        })
    }

    /// The workspace that produced the spec (not linked: always null).
    async fn workspace(&self) -> Option<VisibleBatchSpecWorkspaceNode> {
        None
    }
}

/// A page of changeset specs.
#[derive(SimpleObject)]
pub struct ChangesetSpecConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<ChangesetSpecNode>,
}

/// Nodes for `specs`, hidden where the viewer may not read the repository.
async fn changeset_spec_nodes(
    ctx: &Context<'_>,
    specs: Vec<ChangesetSpec>,
) -> Result<Vec<ChangesetSpecNode>> {
    let ids: Vec<RepoId> = specs.iter().map(|s| s.repo_id).collect();
    let repos = readable_repos(ctx, &ids).await?;
    Ok(specs
        .into_iter()
        .map(|s| match repos.get(&s.repo_id) {
            Some(r) => {
                ChangesetSpecNode::VisibleChangesetSpec(VisibleChangesetSpecNode::new(s, r.clone()))
            }
            None => ChangesetSpecNode::HiddenChangesetSpec(HiddenChangesetSpecNode(Arc::new(s))),
        })
        .collect())
}

/// A changeset spec by ID for `node(id:)` (hidden in unreadable repositories).
pub(super) async fn load_changeset_spec(
    ctx: &Context<'_>,
    id: &str,
) -> Result<Option<ChangesetSpecNode>> {
    let Some(rand) = parse_string_id(CHANGESET_SPEC_KIND, id) else {
        return Ok(None);
    };
    let Some(spec) = ChangesetSpecStore::new(&services(ctx)?.db)
        .get_by_rand_id(&rand)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    Ok(changeset_spec_nodes(ctx, vec![spec]).await?.pop())
}

/// `createChangesetSpec(s)`: validates every spec and checks its repository is readable,
/// then stores them all in one transaction.
pub(super) async fn create_changeset_specs(
    ctx: &Context<'_>,
    raws: &[String],
) -> Result<Vec<ChangesetSpecNode>> {
    let actor = require_write(ctx)?;
    let mut news = Vec::with_capacity(raws.len());
    let mut repos = Vec::with_capacity(raws.len());
    for raw in raws {
        let parsed = sourcegull_batches::parse_changeset_spec(raw).map_err(|e| spec_errors(&e))?;
        let repo_id = parse_repository_id(parsed.base_repository())?;
        // An unreadable repository is "not found" like a missing one.
        let repo = readable_repo(ctx, repo_id)
            .await?
            .ok_or_else(|| not_found("Repository"))?;
        news.push(new_changeset_spec(parsed, repo_id, actor.user_id));
        repos.push(repo);
    }
    let created = ChangesetSpecStore::new(&services(ctx)?.db)
        .create_many(&news)
        .await
        .map_err(db_error)?;
    Ok(created
        .into_iter()
        .zip(repos)
        .map(|(s, r)| ChangesetSpecNode::VisibleChangesetSpec(VisibleChangesetSpecNode::new(s, r)))
        .collect())
}

// ------------------------------------------------------------------ workspaces

/// The resolution of a server-side spec's workspaces.
pub struct WorkspaceResolutionNode {
    job: ResolutionJobRow,
    spec: Arc<BatchSpec>,
}

/// Counts of a spec's workspaces by execution state.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspacesStats")]
pub struct WorkspacesStats {
    pub errored: i32,
    pub completed: i32,
    pub processing: i32,
    pub queued: i32,
    pub ignored: i32,
}

/// A page of workspaces.
#[derive(SimpleObject)]
#[graphql(name = "BatchSpecWorkspaceConnection")]
pub struct WorkspaceConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<BatchSpecWorkspaceNode>,
    pub stats: WorkspacesStats,
}

#[Object(name = "BatchSpecWorkspaceResolution")]
impl WorkspaceResolutionNode {
    /// Why the resolution failed, if it did.
    async fn failure_message(&self) -> Option<&str> {
        self.job.failure_message.as_deref()
    }

    async fn started_at(&self) -> Option<DateTime<Utc>> {
        self.job.started_at
    }

    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.job.finished_at
    }

    async fn state(&self) -> ResolutionStateGql {
        match self.job.state {
            JobState::Queued if self.job.num_failures > 0 => ResolutionStateGql::Errored,
            JobState::Queued => ResolutionStateGql::Queued,
            JobState::Processing => ResolutionStateGql::Processing,
            JobState::Errored => ResolutionStateGql::Errored,
            JobState::Failed | JobState::Canceled => ResolutionStateGql::Failed,
            JobState::Completed => ResolutionStateGql::Completed,
        }
    }

    /// The resolved workspaces, in id order (`orderBy` is accepted but ignored).
    async fn workspaces(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        order_by: Option<WorkspacesSortOrder>,
        search: Option<String>,
        state: Option<WorkspaceStateGql>,
    ) -> Result<WorkspaceConnection> {
        let _ = order_by;
        let limit = check_first(first)?;
        let db = &services(ctx)?.db;
        let store = BatchSpecWorkspaceStore::new(db);
        let mut opts = ListWorkspaces {
            batch_spec_id: self.spec.id,
            state: state.map(|s| s.filter().to_owned()),
            search: search.filter(|s| !s.trim().is_empty()),
            ..Default::default()
        };
        let total = store.count(&opts).await.map_err(db_error)?;
        let exec = store.stats(self.spec.id).await.map_err(db_error)?;
        let ignored: i64 = sourcegull_db::sqlx::query_scalar(
            "SELECT count(*) FROM batch_spec_workspaces WHERE batch_spec_id = $1 AND ignored",
        )
        .bind(self.spec.id)
        .fetch_one(db.pool())
        .await
        .map_err(|e| db_error(e.into()))?;
        let stats = WorkspacesStats {
            errored: to_int(exec.failed),
            completed: to_int(exec.completed),
            processing: to_int(exec.processing),
            queued: to_int(exec.queued),
            ignored: to_int(ignored),
        };
        if limit == 0 {
            return Ok(WorkspaceConnection {
                total_count: to_int(total),
                page_info: PageInfo {
                    has_next_page: total > 0,
                    end_cursor: None,
                },
                nodes: Vec::new(),
                stats,
            });
        }
        opts.after_id = parse_cursor(after.as_deref())?;
        opts.limit = limit + 1;
        let mut rows = store.list(&opts).await.map_err(db_error)?;
        let has_next_page = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        rows.truncate(usize::try_from(limit).unwrap_or(0));
        let end_cursor = has_next_page
            .then(|| rows.last().map(|w| w.id.to_string()))
            .flatten();
        Ok(WorkspaceConnection {
            total_count: to_int(total),
            page_info: PageInfo {
                has_next_page,
                end_cursor,
            },
            nodes: workspace_nodes(ctx, self.spec.clone(), rows).await?,
            stats,
        })
    }
}

/// What both workspace types share.
#[derive(Clone)]
pub(super) struct WorkspaceData {
    pub(super) ws: Arc<BatchSpecWorkspace>,
    pub(super) job: Option<Arc<ExecutionJobRow>>,
    pub(super) spec: Arc<BatchSpec>,
}

impl WorkspaceData {
    fn id(&self) -> ID {
        ID(marshal_id(WORKSPACE_KIND, self.ws.id))
    }

    fn state(&self) -> WorkspaceStateGql {
        if self.ws.skipped {
            return WorkspaceStateGql::Skipped;
        }
        let Some(job) = &self.job else {
            return if self.ws.cached_result_found {
                WorkspaceStateGql::Completed
            } else {
                WorkspaceStateGql::Pending
            };
        };
        match job.state {
            JobState::Queued | JobState::Errored => WorkspaceStateGql::Queued,
            JobState::Processing if job.cancel => WorkspaceStateGql::Canceling,
            JobState::Processing => WorkspaceStateGql::Processing,
            JobState::Completed => WorkspaceStateGql::Completed,
            JobState::Failed => WorkspaceStateGql::Failed,
            JobState::Canceled => WorkspaceStateGql::Canceled,
        }
    }

    fn step_cache_result_count(&self) -> i32 {
        to_int(
            self.ws
                .step_cache_results
                .as_object()
                .map_or(0, |o| i64::try_from(o.len()).unwrap_or(i64::MAX)),
        )
    }

    fn queued(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|j| matches!(j.state, JobState::Queued | JobState::Errored))
    }

    async fn diff_stat(&self, ctx: &Context<'_>) -> Result<Option<DiffStat>> {
        if self.ws.changeset_spec_ids.is_empty() {
            return Ok(None);
        }
        let specs = ChangesetSpecStore::new(&services(ctx)?.db)
            .list(&ListChangesetSpecs {
                ids: Some(self.ws.changeset_spec_ids.clone()),
                ..Default::default()
            })
            .await
            .map_err(db_error)?;
        let (added, deleted) = specs.iter().fold((0i64, 0i64), |(a, d), s| {
            (
                a + i64::from(s.diff_stat_added.unwrap_or(0)),
                d + i64::from(s.diff_stat_deleted.unwrap_or(0)),
            )
        });
        Ok(Some(DiffStat {
            added: to_int(added),
            deleted: to_int(deleted),
            changed: 0,
        }))
    }
}

/// A workspace in a repository the viewer may read.
pub struct VisibleBatchSpecWorkspaceNode {
    d: WorkspaceData,
    repo: Arc<Repo>,
}

/// A workspace in a repository the viewer may not read.
pub struct HiddenBatchSpecWorkspaceNode {
    d: WorkspaceData,
}

/// A workspace of a bulk change spec (`BatchSpecWorkspace` interface).
#[allow(clippy::duplicated_attributes)]
#[derive(Interface)]
#[graphql(
    name = "BatchSpecWorkspace",
    field(name = "id", ty = "ID", desc = "The opaque node ID."),
    field(name = "batch_spec", ty = "BatchSpecNode"),
    field(name = "only_fetch_workspace", ty = "bool"),
    field(name = "ignored", ty = "bool"),
    field(name = "unsupported", ty = "bool"),
    field(name = "cached_result_found", ty = "bool"),
    field(name = "step_cache_result_count", ty = "i32"),
    field(name = "queued_at", ty = "Option<DateTime<Utc>>"),
    field(name = "started_at", ty = "Option<DateTime<Utc>>"),
    field(name = "finished_at", ty = "Option<DateTime<Utc>>"),
    field(name = "state", ty = "WorkspaceStateGql"),
    field(name = "place_in_queue", ty = "Option<i32>"),
    field(name = "place_in_global_queue", ty = "Option<i32>"),
    field(name = "diff_stat", ty = "Option<DiffStat>")
)]
pub enum BatchSpecWorkspaceNode {
    VisibleBatchSpecWorkspace(VisibleBatchSpecWorkspaceNode),
    HiddenBatchSpecWorkspace(HiddenBatchSpecWorkspaceNode),
}

/// Implements the fields both workspace types share.
macro_rules! workspace_fields {
    ($t:ty, $name:literal { $($extra:tt)* }) => {
        #[Object(name = $name)]
        impl $t {
            /// The opaque node ID.
            pub async fn id(&self) -> ID {
                self.d.id()
            }

            /// The spec the workspace belongs to.
            async fn batch_spec(&self) -> BatchSpecNode {
                BatchSpecNode((*self.d.spec).clone())
            }

            /// Only the workspace's files are fetched (`workspaces[].onlyFetchWorkspace`).
            async fn only_fetch_workspace(&self) -> bool {
                self.d.ws.only_fetch_workspace
            }

            /// The repository has a `.batchignore` (skipped unless `allowIgnored`).
            async fn ignored(&self) -> bool {
                self.d.ws.ignored
            }

            /// The repository's code host is not supported (skipped unless
            /// `allowUnsupported`).
            async fn unsupported(&self) -> bool {
                self.d.ws.unsupported
            }

            /// The whole result came from the execution cache.
            async fn cached_result_found(&self) -> bool {
                self.d.ws.cached_result_found
            }

            /// Steps whose result came from the cache.
            async fn step_cache_result_count(&self) -> i32 {
                self.d.step_cache_result_count()
            }

            async fn queued_at(&self) -> Option<DateTime<Utc>> {
                self.d.job.as_ref().map(|j| j.queued_at)
            }

            async fn started_at(&self) -> Option<DateTime<Utc>> {
                self.d.job.as_ref().and_then(|j| j.started_at)
            }

            async fn finished_at(&self) -> Option<DateTime<Utc>> {
                self.d.job.as_ref().and_then(|j| j.finished_at)
            }

            async fn state(&self) -> WorkspaceStateGql {
                self.d.state()
            }

            /// The position among the user's queued executions (1 = next).
            async fn place_in_queue(&self) -> Option<i32> {
                if !self.d.queued() {
                    return None;
                }
                self.d.job.as_ref()?.payload.place_in_user_queue.map(to_int)
            }

            /// The position among all queued executions (1 = next).
            async fn place_in_global_queue(&self) -> Option<i32> {
                if !self.d.queued() {
                    return None;
                }
                self.d.job.as_ref()?.payload.place_in_global_queue.map(to_int)
            }

            /// The summed diff stat of the changeset specs the workspace produced.
            async fn diff_stat(&self, ctx: &Context<'_>) -> Result<Option<DiffStat>> {
                self.d.diff_stat(ctx).await
            }

            $($extra)*
        }
    };
}

workspace_fields!(HiddenBatchSpecWorkspaceNode, "HiddenBatchSpecWorkspace" {});

workspace_fields!(VisibleBatchSpecWorkspaceNode, "VisibleBatchSpecWorkspace" {
    /// The repository.
    async fn repository(&self) -> RepositoryRef {
        RepositoryRef((*self.repo).clone())
    }

    /// The branch the workspace was resolved on, at the resolved commit.
    async fn branch(&self) -> BranchRef {
        BranchRef(BranchData {
            repo: (*self.repo).clone(),
            name: self.d.ws.branch.clone(),
            commit_oid: self.d.ws.commit.clone(),
        })
    }

    /// The workspace's directory ('' for the repository root).
    async fn path(&self) -> &str {
        &self.d.ws.path
    }

    /// The files of the `on` search results in the workspace.
    async fn search_result_paths(&self) -> &[String] {
        &self.d.ws.file_matches
    }

    /// Why the execution failed, if it did.
    async fn failure_message(&self) -> Option<&str> {
        self.d.job.as_ref().and_then(|j| j.failure_message.as_deref())
    }

    /// The execution's log entries by stage; null before the workspace is executed.
    async fn stages(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<super::execution::WorkspaceStages>> {
        super::execution::stages(ctx, &self.d).await
    }

    /// The spec's steps with what the execution (or the cache) knows about them.
    async fn steps(&self, ctx: &Context<'_>) -> Result<Vec<super::execution::WorkspaceStepNode>> {
        super::execution::steps(ctx, &self.d, &self.repo).await
    }

    /// Step `index`, from 1.
    async fn step(
        &self,
        ctx: &Context<'_>,
        index: i32,
    ) -> Result<Option<super::execution::WorkspaceStepNode>> {
        let Some(i) = usize::try_from(index).ok().and_then(|i| i.checked_sub(1)) else {
            return Ok(None);
        };
        let mut steps = super::execution::steps(ctx, &self.d, &self.repo).await?;
        Ok((i < steps.len()).then(|| steps.swap_remove(i)))
    }

    /// The executor running (or that ran) the execution; site admins only.
    async fn executor(&self, ctx: &Context<'_>) -> Result<Option<ExecutorRef>> {
        super::execution::executor(ctx, &self.d).await
    }

    /// The changeset specs the execution produced.
    async fn changeset_specs(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<Vec<VisibleChangesetSpecNode>>> {
        if self.d.ws.changeset_spec_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let specs = ChangesetSpecStore::new(&services(ctx)?.db)
            .list(&ListChangesetSpecs {
                ids: Some(self.d.ws.changeset_spec_ids.clone()),
                ..Default::default()
            })
            .await
            .map_err(db_error)?;
        Ok(Some(
            specs
                .into_iter()
                .filter(|s| s.repo_id == self.repo.id)
                .map(|s| VisibleChangesetSpecNode::new(s, (*self.repo).clone()))
                .collect(),
        ))
    }
});

/// Workspace nodes for `rows` of `spec`, with their latest executions; hidden where the
/// viewer may not read the repository.
async fn workspace_nodes(
    ctx: &Context<'_>,
    spec: Arc<BatchSpec>,
    rows: Vec<BatchSpecWorkspace>,
) -> Result<Vec<BatchSpecWorkspaceNode>> {
    let ids: Vec<i64> = rows.iter().map(|w| w.id).collect();
    let jobs: HashMap<i64, Arc<ExecutionJobRow>> = BatchSpecWorkspaceStore::new(&services(ctx)?.db)
        .execution_jobs_for_workspaces(&ids)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|j| (j.payload.batch_spec_workspace_id, Arc::new(j)))
        .collect();
    let repo_ids: Vec<RepoId> = rows.iter().map(|w| w.repo_id).collect();
    let repos = readable_repos(ctx, &repo_ids).await?;
    Ok(rows
        .into_iter()
        .map(|w| {
            let d = WorkspaceData {
                job: jobs.get(&w.id).cloned(),
                spec: spec.clone(),
                ws: Arc::new(w),
            };
            match repos.get(&d.ws.repo_id) {
                Some(r) => BatchSpecWorkspaceNode::VisibleBatchSpecWorkspace(
                    VisibleBatchSpecWorkspaceNode {
                        repo: Arc::new(r.clone()),
                        d,
                    },
                ),
                None => {
                    BatchSpecWorkspaceNode::HiddenBatchSpecWorkspace(HiddenBatchSpecWorkspaceNode {
                        d,
                    })
                }
            }
        })
        .collect())
}

/// A workspace by ID for `node(id:)`.
pub(super) async fn load_workspace(
    ctx: &Context<'_>,
    id: &str,
) -> Result<Option<BatchSpecWorkspaceNode>> {
    let Some(key) = unmarshal_int_id(WORKSPACE_KIND, id) else {
        return Ok(None);
    };
    let db = &services(ctx)?.db;
    let Some(ws) = BatchSpecWorkspaceStore::new(db)
        .get(key)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    let Some(spec) = BatchSpecStore::new(db)
        .get(ws.batch_spec_id)
        .await
        .map_err(db_error)?
    else {
        return Ok(None);
    };
    Ok(workspace_nodes(ctx, Arc::new(spec), vec![ws]).await?.pop())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_states() {
        use BatchSpecStateGql as S;
        let stats = |workspaces, executions, queued, completed, failed| ExecutionStats {
            workspaces,
            executions,
            queued,
            completed,
            failed,
            ..Default::default()
        };
        assert_eq!(spec_state(None, &stats(0, 0, 0, 0, 0), false), S::Pending);
        assert_eq!(
            spec_state(Some(JobState::Queued), &stats(0, 0, 0, 0, 0), false),
            S::Pending
        );
        assert_eq!(
            spec_state(Some(JobState::Failed), &stats(0, 0, 0, 0, 0), false),
            S::Failed
        );
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(0, 0, 0, 0, 0), false),
            S::Completed
        );
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(2, 0, 0, 0, 0), false),
            S::Pending
        );
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(2, 2, 2, 0, 0), false),
            S::Queued
        );
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(2, 2, 0, 1, 1), false),
            S::Failed
        );
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(2, 2, 1, 1, 0), false),
            S::Processing
        );
        // Executed with every workspace cached: no jobs.
        assert_eq!(
            spec_state(Some(JobState::Completed), &stats(2, 0, 0, 0, 0), true),
            S::Completed
        );
    }
}
