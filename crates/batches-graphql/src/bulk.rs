//! Bulk operations: the
//! `createChangesetComments`, `reenqueueChangesets`, `mergeChangesets`, `closeChangesets`,
//! `publishChangesets` and `detachChangesets` mutations, the `BulkOperation` node,
//! `BatchChange.bulkOperations` and `BatchChange.availableBulkOperations`.
//!
//! A mutation queues one `changeset_jobs` row per changeset (a bulk operation, carried out by
//! the worker role, `sourcegull_batches_reconciler::bulk`). The viewer must be
//! able to administer the bulk change (its creator, a site admin, or an organization member
//! with `orgs.allMembersBatchChangesAdmin`), and every changeset must belong to the batch
//! change, be in a repository the viewer may read and be eligible for the operation (e.g.
//! only open, published, reconciled changesets can be merged; only archived ones detached);
//! otherwise nothing is queued and the mutation fails with "some changesets could not be
//! found".

use async_graphql::{Context, Enum, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use serde_json::json;
use sourcegull_batches::store::{
    ArchivedFilter, BatchChangeStore, BulkOperation, Changeset, ChangesetJobStore,
    ChangesetJobType, ChangesetStore, ExternalState, ListChangesets, PublicationState,
    ReconcilerState,
};
use sourcegull_services::relay::{marshal_string_id, unmarshal_int_id};

use super::changeset::ChangesetNode;
use super::{
    CHANGESET_KIND, audit, check_first, invalid, invalid_id, not_found, parse_batch_change_id,
    parse_string_id, readable_repos, require_can_modify, require_read, require_write,
};
use crate::context::{codes, db_error, error, repo_access, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::shared::UserRef;

/// Node kind of bulk operation IDs (keyed by the bulk group).
pub const BULK_OPERATION_KIND: &str = "BulkOperation";

/// The kind of a bulk operation.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BulkOperationType")]
pub enum BulkOperationType {
    #[graphql(name = "COMMENT")]
    Comment,
    #[graphql(name = "DETACH")]
    Detach,
    #[graphql(name = "REENQUEUE")]
    Reenqueue,
    #[graphql(name = "MERGE")]
    Merge,
    #[graphql(name = "CLOSE")]
    Close,
    #[graphql(name = "PUBLISH")]
    Publish,
}

impl From<ChangesetJobType> for BulkOperationType {
    fn from(t: ChangesetJobType) -> Self {
        match t {
            ChangesetJobType::Comment => Self::Comment,
            ChangesetJobType::Detach => Self::Detach,
            ChangesetJobType::Reenqueue => Self::Reenqueue,
            ChangesetJobType::Merge => Self::Merge,
            ChangesetJobType::Close => Self::Close,
            ChangesetJobType::Publish => Self::Publish,
        }
    }
}

/// The state of a bulk operation.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "BulkOperationState")]
pub enum BulkOperationState {
    #[graphql(name = "PROCESSING")]
    Processing,
    #[graphql(name = "FAILED")]
    Failed,
    #[graphql(name = "COMPLETED")]
    Completed,
}

/// The ID of a bulk operation.
pub fn bulk_operation_id(group: &str) -> ID {
    ID(marshal_string_id(BULK_OPERATION_KIND, group))
}

/// A bulk operation.
pub struct BulkOperationNode(pub BulkOperation);

/// A changeset a bulk operation failed on.
pub struct ChangesetJobError {
    changeset: Changeset,
    error: String,
    visible: bool,
    repo: Option<sourcegull_types::Repo>,
}

/// A changeset a bulk operation failed on.
#[Object(name = "ChangesetJobError")]
impl ChangesetJobError {
    /// The changeset.
    async fn changeset(&self) -> ChangesetNode {
        ChangesetNode::new(self.changeset.clone(), self.repo.clone())
    }

    /// The error; null when the changeset is in a repository the viewer may not read.
    async fn error(&self) -> Option<&str> {
        self.visible.then_some(self.error.as_str())
    }
}

#[Object(name = "BulkOperation")]
impl BulkOperationNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        bulk_operation_id(&self.0.id)
    }

    /// What the operation does.
    #[graphql(name = "type")]
    async fn kind(&self) -> BulkOperationType {
        self.0.job_type.into()
    }

    /// Processing while a changeset is left, then failed if one failed, else completed.
    async fn state(&self) -> BulkOperationState {
        match self.0.state {
            "PROCESSING" => BulkOperationState::Processing,
            "FAILED" => BulkOperationState::Failed,
            _ => BulkOperationState::Completed,
        }
    }

    /// The share of changesets processed, from 0 to 1.
    async fn progress(&self) -> f64 {
        self.0.progress
    }

    /// The changesets the operation failed on, with the errors.
    async fn errors(&self, ctx: &Context<'_>) -> Result<Vec<ChangesetJobError>> {
        let db = &services(ctx)?.db;
        let errors = ChangesetJobStore::new(db)
            .errors(&self.0.id)
            .await
            .map_err(db_error)?;
        let ids: Vec<i64> = errors.iter().map(|(id, _)| *id).collect();
        let changesets = ChangesetStore::new(db)
            .get_many(&ids)
            .await
            .map_err(db_error)?;
        let repo_ids: Vec<_> = changesets.iter().map(|c| c.repo_id).collect();
        let repos = readable_repos(ctx, &repo_ids).await?;
        Ok(errors
            .into_iter()
            .filter_map(|(id, error)| {
                let changeset = changesets.iter().find(|c| c.id == id)?.clone();
                let repo = repos.get(&changeset.repo_id).cloned();
                Some(ChangesetJobError {
                    visible: repo.is_some(),
                    repo,
                    changeset,
                    error,
                })
            })
            .collect())
    }

    /// The user who started the operation.
    async fn initiator(&self, ctx: &Context<'_>) -> Result<UserRef> {
        super::user_node(ctx, Some(self.0.user_id))
            .await?
            .ok_or_else(|| not_found("User"))
    }

    /// The number of changesets the operation runs on.
    async fn changeset_count(&self) -> i32 {
        to_int(self.0.changeset_count)
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    /// When the last changeset was processed; null while processing.
    async fn finished_at(&self) -> Option<DateTime<Utc>> {
        self.0.finished_at
    }
}

/// A page of bulk operations.
#[derive(SimpleObject)]
pub struct BulkOperationConnection {
    pub nodes: Vec<BulkOperationNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// `BatchChange.bulkOperations`: newest first, optionally only those created after
/// `created_after`. The cursor is the creation time of the last one of the previous page.
pub(super) async fn list(
    ctx: &Context<'_>,
    batch_change_id: i64,
    first: Option<i32>,
    after: Option<String>,
    created_after: Option<DateTime<Utc>>,
) -> Result<BulkOperationConnection> {
    let limit = usize::try_from(check_first(first)?).unwrap_or(0);
    let before = match after.as_deref() {
        None | Some("") => None,
        Some(c) => Some(
            DateTime::parse_from_rfc3339(c)
                .map_err(|_| invalid(format!("Invalid cursor {c:?}.")))?
                .with_timezone(&Utc),
        ),
    };
    let all = ChangesetJobStore::new(&services(ctx)?.db)
        .bulk_operations(batch_change_id, None, 0)
        .await
        .map_err(db_error)?;
    let all: Vec<BulkOperation> = all
        .into_iter()
        .filter(|b| created_after.is_none_or(|t| b.created_at > t))
        .collect();
    let total = all.len();
    let mut page: Vec<BulkOperation> = all
        .into_iter()
        .filter(|b| before.is_none_or(|t| b.created_at < t))
        .collect();
    let has_next_page = page.len() > limit;
    page.truncate(limit);
    let end_cursor = has_next_page
        .then(|| {
            page.last().map(|b| {
                b.created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            })
        })
        .flatten();
    Ok(BulkOperationConnection {
        nodes: page.into_iter().map(BulkOperationNode).collect(),
        total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        page_info: PageInfo {
            has_next_page,
            end_cursor,
        },
    })
}

/// Parses changeset IDs (deduplicated, in order).
fn parse_changeset_ids(ids: &[ID]) -> Result<Vec<i64>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let n = unmarshal_int_id(CHANGESET_KIND, id)
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid_id(id))?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

fn changesets_not_found() -> async_graphql::Error {
    error(codes::NOT_FOUND, "some changesets could not be found")
}

/// Which changesets an operation accepts.
fn eligible(job_type: ChangesetJobType) -> ListChangesets {
    let published = Some(PublicationState::Published);
    match job_type {
        // Archived changesets may be commented on too.
        ChangesetJobType::Comment => ListChangesets {
            publication_state: published,
            archived: ArchivedFilter::Include,
            ..Default::default()
        },
        ChangesetJobType::Detach => ListChangesets {
            archived: ArchivedFilter::Only,
            ..Default::default()
        },
        ChangesetJobType::Reenqueue => ListChangesets {
            reconciler_states: vec![ReconcilerState::Failed],
            ..Default::default()
        },
        ChangesetJobType::Merge => ListChangesets {
            publication_state: published,
            reconciler_states: vec![ReconcilerState::Completed],
            external_states: vec![ExternalState::Open],
            ..Default::default()
        },
        ChangesetJobType::Close => ListChangesets {
            publication_state: published,
            reconciler_states: vec![ReconcilerState::Completed],
            external_states: vec![ExternalState::Open, ExternalState::Draft],
            ..Default::default()
        },
        ChangesetJobType::Publish => ListChangesets::default(),
    }
}

/// Checks access and eligibility and queues the bulk operation.
pub(super) async fn create(
    ctx: &Context<'_>,
    batch_change: &ID,
    changesets: &[ID],
    job_type: ChangesetJobType,
    payload: serde_json::Value,
) -> Result<BulkOperationNode> {
    let actor = require_write(ctx)?.clone();
    let bc_id = parse_batch_change_id(batch_change)?;
    let db = &services(ctx)?.db;
    let bc = BatchChangeStore::new(db)
        .get(bc_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change"))?;
    require_can_modify(ctx, &actor, &bc).await?;
    if changesets.is_empty() {
        return Err(invalid("no changesets provided"));
    }
    let ids = parse_changeset_ids(changesets)?;
    let mut opts = eligible(job_type);
    opts.batch_change_id = Some(bc.id);
    opts.ids = Some(ids.clone());
    let access = repo_access(ctx).await?;
    let found = ChangesetStore::new(db)
        .count(&opts, &access)
        .await
        .map_err(db_error)?;
    if usize::try_from(found).unwrap_or(0) != ids.len() {
        return Err(changesets_not_found());
    }
    let group = ChangesetJobStore::new(db)
        .create_bulk(actor.user_id, bc.id, job_type, &payload, &ids)
        .await
        .map_err(db_error)?;
    audit(
        ctx,
        "BatchChangeBulkOperationCreated",
        &actor,
        json!({"batchChange": bc.id, "type": job_type.as_str(), "changesets": ids.len()}),
    )
    .await;
    ChangesetJobStore::new(db)
        .bulk_operation(&group)
        .await
        .map_err(db_error)?
        .map(BulkOperationNode)
        .ok_or_else(|| not_found("Bulk operation"))
}

/// The operations `c` allows in bulk change `bc`.
fn operations_for(c: &Changeset, bc: i64) -> Vec<BulkOperationType> {
    let ext = c.external_state;
    // No operations on read-only changesets.
    if ext == Some(ExternalState::ReadOnly) {
        return Vec::new();
    }
    let archived = c.archived_in(bc);
    let failed = c.reconciler_state == ReconcilerState::Failed;
    let open = ext == Some(ExternalState::Open);
    let closable = matches!(ext, Some(ExternalState::Open | ExternalState::Draft));
    let commentable = matches!(
        ext,
        Some(
            ExternalState::Open
                | ExternalState::Draft
                | ExternalState::Merged
                | ExternalState::Closed
        )
    );
    let mut ops = Vec::new();
    if commentable {
        ops.push(BulkOperationType::Comment);
    }
    if archived {
        ops.push(BulkOperationType::Detach);
    }
    if !archived && failed {
        ops.push(BulkOperationType::Reenqueue);
    }
    if !archived && !failed && open {
        ops.push(BulkOperationType::Merge);
    }
    if !archived && closable {
        ops.push(BulkOperationType::Close);
    }
    if !archived && c.owned_by_batch_change_id.is_some() {
        ops.push(BulkOperationType::Publish);
    }
    ops
}

/// `BatchChange.availableBulkOperations`: the operations every one of `changesets` allows.
pub(super) async fn available(
    ctx: &Context<'_>,
    batch_change_id: i64,
    changesets: &[ID],
) -> Result<Vec<BulkOperationType>> {
    require_read(ctx)?;
    let ids = parse_changeset_ids(changesets)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let opts = ListChangesets {
        batch_change_id: Some(batch_change_id),
        ids: Some(ids.clone()),
        archived: ArchivedFilter::Include,
        ..Default::default()
    };
    let access = repo_access(ctx).await?;
    let found = ChangesetStore::new(&services(ctx)?.db)
        .list(&opts, &access)
        .await
        .map_err(db_error)?;
    if found.len() != ids.len() {
        return Err(changesets_not_found());
    }
    let per: Vec<Vec<BulkOperationType>> = found
        .iter()
        .map(|c| operations_for(c, batch_change_id))
        .collect();
    let all = [
        BulkOperationType::Comment,
        BulkOperationType::Detach,
        BulkOperationType::Reenqueue,
        BulkOperationType::Merge,
        BulkOperationType::Close,
        BulkOperationType::Publish,
    ];
    Ok(all
        .into_iter()
        .filter(|op| per.iter().all(|ops| ops.contains(op)))
        .collect())
}

/// A bulk operation by ID for `node(id:)`.
pub(super) async fn load_node(ctx: &Context<'_>, id: &str) -> Result<Option<BulkOperationNode>> {
    let Some(group) = parse_string_id(BULK_OPERATION_KIND, id) else {
        return Ok(None);
    };
    Ok(ChangesetJobStore::new(&services(ctx)?.db)
        .bulk_operation(&group)
        .await
        .map_err(db_error)?
        .map(BulkOperationNode))
}
