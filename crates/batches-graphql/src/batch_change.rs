//! `BatchChange`: the node, its changesets and stats, listings, and the mutations that create
//! (empty), close and delete bulk changes.

use async_graphql::{Context, ID, Object, Result, SimpleObject};
use chrono::{DateTime, Utc};
use sourcegull_batches::store::{
    ArchivedFilter, BatchChange, BatchChangeStore, BatchSpecStore, ChangesetCounts, ChangesetStore,
    ListBatchChanges, ListChangesets, NewBatchChange, NewBatchSpec, PublicationState, burndown,
};
use sourcegull_db::Namespace;
use sourcegull_services::relay::unmarshal_int_id;
use sourcegull_types::RepoAccess;

use super::batch_spec::{self, BatchSpecConnection, BatchSpecNode};
use super::bulk::{self, BulkOperationConnection, BulkOperationType};
use super::changeset::{self, ChangesetConnection};
use super::{
    BatchChangeStateGql, ChangesetCheckStateGql, ChangesetReviewStateGql, ChangesetStateGql,
    batch_change_id, batch_codes, can_administer, check_first, invalid, namespace_node,
    namespace_url, not_found, parse_batch_change_id, parse_cursor, require_can_modify,
    require_namespace_access, require_read, require_write, user_node,
};
use crate::context::{codes, db_error, error, repo_access, services, to_int};
use crate::pagination::PageInfo;
use sourcegull_graphql_core::diff::DiffStat;
use sourcegull_graphql_core::ids::parse_namespace_id;
use sourcegull_graphql_core::ids::parse_repository_id;
use sourcegull_graphql_core::shared::{NamespaceRef, UserRef};

/// A bulk change.
#[derive(Clone)]
pub struct BatchChangeNode(pub BatchChange);

impl BatchChangeNode {
    pub fn new(bc: BatchChange) -> Self {
        Self(bc)
    }
}

/// Changeset counts of a bulk change by state (`ChangesetsStats`).
#[derive(SimpleObject)]
#[graphql(complex, name = "ChangesetsStats")]
pub struct ChangesetsStats {
    pub unpublished: i32,
    pub draft: i32,
    pub open: i32,
    pub merged: i32,
    pub closed: i32,
    pub deleted: i32,
    pub retrying: i32,
    pub failed: i32,
    pub scheduled: i32,
    pub processing: i32,
    pub archived: i32,
    /// Every changeset, archived ones included.
    pub total: i32,
}

#[async_graphql::ComplexObject]
impl ChangesetsStats {
    /// Whether every changeset that is not archived or deleted is merged or closed.
    async fn is_completed(&self) -> bool {
        self.total != 0 && self.merged + self.closed == self.total - self.archived - self.deleted
    }

    /// The share of changesets (not archived or deleted) that are merged or closed, in percent.
    async fn percent_complete(&self) -> i32 {
        let included = self.total - self.archived - self.deleted;
        if included <= 0 {
            return 100;
        }
        let done = f64::from(self.merged + self.closed);
        // Truncated, not rounded.
        #[allow(clippy::cast_possible_truncation)]
        let pct = (done / f64::from(included) * 100.0) as i32;
        pct
    }
}

impl From<ChangesetCounts> for ChangesetsStats {
    fn from(c: ChangesetCounts) -> Self {
        Self {
            unpublished: to_int(c.unpublished),
            draft: to_int(c.draft),
            open: to_int(c.open),
            merged: to_int(c.merged),
            closed: to_int(c.closed),
            deleted: to_int(c.deleted),
            retrying: to_int(c.retrying),
            failed: to_int(c.failed),
            scheduled: to_int(c.scheduled),
            processing: to_int(c.processing),
            archived: to_int(c.archived),
            total: to_int(c.total + c.archived),
        }
    }
}

/// The `changeset_events` kinds the burndown reads.
const BURNDOWN_EVENT_KINDS: &[&str] = &[
    "CLOSED",
    "REOPENED",
    "MERGED",
    "MARKED_DRAFT",
    "MARKED_READY",
    "APPROVED",
    "UNAPPROVED",
    "CHANGES_REQUESTED",
];

/// Changeset counts at one point of the burndown chart.
#[derive(SimpleObject)]
#[graphql(name = "ChangesetCounts")]
pub struct ChangesetCountsNode {
    pub date: DateTime<Utc>,
    pub total: i32,
    pub merged: i32,
    pub closed: i32,
    pub draft: i32,
    pub open: i32,
    pub open_approved: i32,
    pub open_changes_requested: i32,
    pub open_pending: i32,
}

impl From<burndown::DailyChangesetCounts> for ChangesetCountsNode {
    fn from(c: burndown::DailyChangesetCounts) -> Self {
        Self {
            date: c.date,
            total: c.total,
            merged: c.merged,
            closed: c.closed,
            draft: c.draft,
            open: c.open,
            open_approved: c.open_approved,
            open_changes_requested: c.open_changes_requested,
            open_pending: c.open_pending,
        }
    }
}

/// The arguments of a `changesets` listing of a bulk change.
#[derive(Default)]
pub struct ChangesetsArgs {
    pub first: Option<i32>,
    pub after: Option<String>,
    pub state: Option<ChangesetStateGql>,
    pub only_closable: Option<bool>,
    pub review_state: Option<ChangesetReviewStateGql>,
    pub check_state: Option<ChangesetCheckStateGql>,
    pub only_published_by_this_batch_change: Option<bool>,
    pub search: Option<String>,
    pub only_archived: bool,
    pub repo: Option<ID>,
}

/// The store filter of `args` in bulk change `id`, and whether it is safe to include hidden
/// changesets: filters on review state, check state, text or repository could reveal something
/// about a changeset the viewer may not see.
fn changeset_filter(id: i64, args: &ChangesetsArgs) -> Result<(ListChangesets, bool)> {
    let mut opts = ListChangesets {
        batch_change_id: Some(id),
        ..Default::default()
    };
    let mut safe = true;
    if args.only_closable == Some(true) {
        if args.state.is_some() {
            return Err(invalid("invalid combination of state and onlyClosable"));
        }
        opts.only_closable = true;
    }
    if let Some(s) = args.state {
        opts.states = vec![s.into()];
    }
    if let Some(s) = args.review_state {
        opts.review_state = Some(s.into());
        safe = false;
    }
    if let Some(s) = args.check_state {
        opts.check_state = Some(s.db());
        safe = false;
    }
    if args.only_published_by_this_batch_change == Some(true) {
        opts.owned_by_batch_change = true;
        opts.publication_state = Some(PublicationState::Published);
    }
    if let Some(q) = args.search.as_deref().filter(|q| !q.trim().is_empty()) {
        opts.search = Some(q.to_owned());
        safe = false;
    }
    if args.only_archived {
        opts.archived = ArchivedFilter::Only;
    }
    if let Some(repo) = &args.repo {
        opts.repo = Some(parse_repository_id(repo)?);
        // Would tell which hidden changesets are in which (guessable) repository ID.
        safe = false;
    }
    Ok((opts, safe))
}

#[Object(name = "BatchChange")]
impl BatchChangeNode {
    /// The opaque node ID.
    pub async fn id(&self) -> ID {
        batch_change_id(self.0.id)
    }

    /// The user or organization that owns the bulk change.
    async fn namespace(&self, ctx: &Context<'_>) -> Result<NamespaceRef> {
        namespace_node(ctx, self.0.namespace).await
    }

    /// The name, unique in the namespace.
    async fn name(&self) -> &str {
        &self.0.name
    }

    /// The description (Markdown), if any.
    async fn description(&self) -> Option<&str> {
        Some(self.0.description.as_str()).filter(|d| !d.is_empty())
    }

    /// Open, closed, or a draft that was never applied.
    async fn state(&self) -> BatchChangeStateGql {
        self.0.state().into()
    }

    /// The creator; null once deleted.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user_node(ctx, self.0.creator_id).await
    }

    /// Who applied the current spec last.
    async fn last_applier(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        user_node(ctx, self.0.last_applier_id).await
    }

    /// Whether the viewer may administer the bulk change (themself, a member of its
    /// organization, or a site admin).
    async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        can_administer(ctx, self.0.namespace).await
    }

    /// The bulk change's page.
    async fn url(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(format!(
            "{}/bulk-changes/{}",
            namespace_url(ctx, self.0.namespace).await?,
            self.0.name
        ))
    }

    async fn created_at(&self) -> DateTime<Utc> {
        self.0.created_at
    }

    async fn updated_at(&self) -> DateTime<Utc> {
        self.0.updated_at
    }

    async fn last_applied_at(&self) -> Option<DateTime<Utc>> {
        self.0.last_applied_at
    }

    async fn closed_at(&self) -> Option<DateTime<Utc>> {
        self.0.closed_at
    }

    /// Changeset counts by state (every changeset, including ones in repositories the viewer
    /// may not read).
    async fn changesets_stats(&self, ctx: &Context<'_>) -> Result<ChangesetsStats> {
        // Counts only, without names: hidden changesets count too (they are
        // listed as `HiddenExternalChangeset`).
        let counts = ChangesetStore::new(&services(ctx)?.db)
            .counts(self.0.id, &RepoAccess::All)
            .await
            .map_err(db_error)?;
        Ok(counts.into())
    }

    /// The changesets, in id order. Without filters on review state, check state or text,
    /// changesets in repositories the viewer may not read are included as
    /// `HiddenExternalChangeset`; with them, they are left out.
    #[allow(clippy::too_many_arguments)]
    async fn changesets(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        state: Option<ChangesetStateGql>,
        only_closable: Option<bool>,
        review_state: Option<ChangesetReviewStateGql>,
        check_state: Option<ChangesetCheckStateGql>,
        only_published_by_this_batch_change: Option<bool>,
        search: Option<String>,
        #[graphql(default_with = "Some(false)")] only_archived: Option<bool>,
        repo: Option<ID>,
    ) -> Result<ChangesetConnection> {
        let args = ChangesetsArgs {
            first,
            after,
            state,
            only_closable,
            review_state,
            check_state,
            only_published_by_this_batch_change,
            search,
            only_archived: only_archived.unwrap_or(false),
            repo,
        };
        let (opts, safe) = changeset_filter(self.0.id, &args)?;
        let limit = check_first(args.first)?;
        let after = parse_cursor(args.after.as_deref())?;
        changeset::connection(ctx, opts, safe, limit, after).await
    }

    /// The summed diff stat of the changesets in repositories the viewer may read.
    async fn diff_stat(&self, ctx: &Context<'_>) -> Result<DiffStat> {
        let access = repo_access(ctx).await?;
        let opts = ListChangesets {
            batch_change_id: Some(self.0.id),
            archived: ArchivedFilter::Include,
            ..Default::default()
        };
        let all = ChangesetStore::new(&services(ctx)?.db)
            .list(&opts, &access)
            .await
            .map_err(db_error)?;
        let (added, deleted) = all.iter().fold((0i64, 0i64), |(a, d), c| {
            (
                a + i64::from(c.diff_stat_added.unwrap_or(0)),
                d + i64::from(c.diff_stat_deleted.unwrap_or(0)),
            )
        });
        Ok(DiffStat {
            added: to_int(added),
            deleted: to_int(deleted),
            changed: 0,
        })
    }

    /// The last applied spec (for a draft, the spec it was created with).
    async fn current_spec(&self, ctx: &Context<'_>) -> Result<BatchSpecNode> {
        BatchSpecStore::new(&services(ctx)?.db)
            .get(self.0.batch_spec_id)
            .await
            .map_err(db_error)?
            .map(BatchSpecNode::new)
            .ok_or_else(|| not_found("Bulk change spec"))
    }

    /// The bulk operations on the bulk change's changesets, newest first.
    async fn bulk_operations(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        created_after: Option<DateTime<Utc>>,
    ) -> Result<BulkOperationConnection> {
        require_read(ctx)?;
        bulk::list(ctx, self.0.id, first, after, created_after).await
    }

    /// The bulk operations that every one of `changesets` (of this bulk change, in
    /// readable repositories) allows.
    async fn available_bulk_operations(
        &self,
        ctx: &Context<'_>,
        changesets: Vec<ID>,
    ) -> Result<Vec<BulkOperationType>> {
        bulk::available(ctx, self.0.id, &changesets).await
    }

    /// The burndown chart: daily counts of the published changesets by state, from `from`
    /// (by default the first event or creation of the bulk change, at least a week ago) to
    /// `to` (by default now), reconstructed from the changesets' timelines. Points are
    /// 24 hours apart, ending at `to`.
    async fn changeset_counts_over_time(
        &self,
        ctx: &Context<'_>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        #[graphql(default_with = "Some(false)")] include_archived: Option<bool>,
    ) -> Result<Vec<ChangesetCountsNode>> {
        require_read(ctx)?;
        let db = &services(ctx)?.db;
        let opts = ListChangesets {
            batch_change_id: Some(self.0.id),
            publication_state: Some(PublicationState::Published),
            archived: if include_archived.unwrap_or(false) {
                ArchivedFilter::Include
            } else {
                ArchivedFilter::Exclude
            },
            ..Default::default()
        };
        // Counts only, like `changesetsStats`: every changeset counts.
        let changesets = ChangesetStore::new(db)
            .list(&opts, &RepoAccess::All)
            .await
            .map_err(db_error)?;
        let ids: Vec<i64> = changesets.iter().map(|c| c.id).collect();
        let kinds: Vec<String> = BURNDOWN_EVENT_KINDS
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        let events = if ids.is_empty() {
            Vec::new()
        } else {
            ChangesetStore::new(db)
                .events(&ids, &kinds)
                .await
                .map_err(db_error)?
        };
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);
        let start = from.unwrap_or_else(|| {
            events
                .iter()
                .map(|e| e.created_at)
                .min()
                .unwrap_or(self.0.created_at)
                .min(week_ago)
        });
        let end = to.filter(|t| *t < now).unwrap_or(now);
        Ok(
            burndown::changeset_counts_over_time(start, end, &changesets, &events)
                .into_iter()
                .map(ChangesetCountsNode::from)
                .collect(),
        )
    }

    /// The changesets as CSV (for the UI's export): the selected
    /// `changesets` if given, filtered like `changesets`, in id order. Changesets in
    /// repositories the viewer may not read are left out. Columns: id, repository, title,
    /// state, review state, check state, external ID and URL, lines added and deleted,
    /// created and updated time.
    #[graphql(name = "changesetsCSV")]
    #[allow(clippy::too_many_arguments)]
    async fn changesets_csv(
        &self,
        ctx: &Context<'_>,
        changesets: Option<Vec<ID>>,
        state: Option<ChangesetStateGql>,
        only_closable: Option<bool>,
        review_state: Option<ChangesetReviewStateGql>,
        check_state: Option<ChangesetCheckStateGql>,
        only_published_by_this_batch_change: Option<bool>,
        search: Option<String>,
        #[graphql(default_with = "Some(false)")] only_archived: Option<bool>,
        repo: Option<ID>,
    ) -> Result<String> {
        require_read(ctx)?;
        let args = ChangesetsArgs {
            state,
            only_closable,
            review_state,
            check_state,
            only_published_by_this_batch_change,
            search,
            only_archived: only_archived.unwrap_or(false),
            repo,
            ..Default::default()
        };
        let (mut opts, _) = changeset_filter(self.0.id, &args)?;
        if let Some(ids) = changesets {
            let mut keys = Vec::with_capacity(ids.len());
            for id in &ids {
                keys.push(
                    unmarshal_int_id(super::CHANGESET_KIND, id)
                        .ok_or_else(|| invalid(format!("Invalid ID {:?}.", id.as_str())))?,
                );
            }
            opts.ids = Some(keys);
        }
        changeset::csv(ctx, &opts).await
    }

    /// The specs created for the bulk change, newest first (server-side ones, plus local
    /// runs' with `includeLocallyExecutedSpecs`).
    async fn batch_specs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 50)] first: Option<i32>,
        after: Option<String>,
        include_locally_executed_specs: Option<bool>,
        exclude_empty_specs: Option<bool>,
    ) -> Result<BatchSpecConnection> {
        batch_spec::list_specs(
            ctx,
            Some(self.0.id),
            first,
            after,
            include_locally_executed_specs.unwrap_or(false),
            exclude_empty_specs.unwrap_or(false),
        )
        .await
    }
}

/// A page of bulk changes.
#[derive(SimpleObject)]
pub struct BatchChangeConnection {
    pub nodes: Vec<BatchChangeNode>,
    pub total_count: i32,
    pub page_info: PageInfo,
}

/// The arguments of a bulk change listing.
pub struct BatchChangesArgs {
    pub first: Option<i32>,
    pub after: Option<String>,
    pub state: Option<BatchChangeStateGql>,
    pub states: Option<Vec<BatchChangeStateGql>>,
    pub viewer_can_administer: Option<bool>,
}

/// Lists bulk changes, in `ns` if given. Non-admins do not see other users' drafts.
pub async fn list_batch_changes(
    ctx: &Context<'_>,
    ns: Option<Namespace>,
    args: BatchChangesArgs,
) -> Result<BatchChangeConnection> {
    let actor = require_read(ctx)?;
    list_with(ctx, actor, ns, None, args).await
}

/// The bulk changes a changeset belongs to.
pub(crate) async fn list_for_changeset(
    ctx: &Context<'_>,
    changeset_id: i64,
    args: BatchChangesArgs,
) -> Result<BatchChangeConnection> {
    let actor = require_read(ctx)?;
    list_with(ctx, actor, None, Some(changeset_id), args).await
}

async fn list_with(
    ctx: &Context<'_>,
    actor: &sourcegull_authn::Actor,
    ns: Option<Namespace>,
    changeset_id: Option<i64>,
    args: BatchChangesArgs,
) -> Result<BatchChangeConnection> {
    let limit = check_first(args.first)?;
    let mut opts = ListBatchChanges {
        namespace: ns,
        changeset_id,
        before_id: parse_cursor(args.after.as_deref())?,
        ..Default::default()
    };
    if let Some(states) = args.states {
        opts.states = states.into_iter().map(Into::into).collect();
    } else if let Some(state) = args.state {
        opts.states = vec![state.into()];
    }
    // Drafts are listed only to their creators (and site admins); a bulk change's own
    // changesets list it regardless.
    sourcegull_services::batches::access::scope_list(
        actor,
        args.viewer_can_administer == Some(true),
        changeset_id.is_some(),
        &mut opts,
    );
    let store = BatchChangeStore::new(&services(ctx)?.db);
    let total = store.count(&opts).await.map_err(db_error)?;
    if limit == 0 {
        return Ok(BatchChangeConnection {
            nodes: Vec::new(),
            total_count: to_int(total),
            page_info: PageInfo {
                has_next_page: total > 0,
                end_cursor: None,
            },
        });
    }
    opts.limit = limit + 1;
    let mut rows = store.list(&opts).await.map_err(db_error)?;
    let has_next_page = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    Ok(BatchChangeConnection {
        page_info: PageInfo {
            has_next_page,
            end_cursor: has_next_page
                .then(|| rows.last().map(|b| b.id.to_string()))
                .flatten(),
        },
        nodes: rows.into_iter().map(BatchChangeNode).collect(),
        total_count: to_int(total),
    })
}

/// `createEmptyBatchChange` / `upsertEmptyBatchChange`.
pub(super) async fn create_empty(
    ctx: &Context<'_>,
    namespace: &str,
    name: &str,
    upsert: bool,
) -> Result<BatchChangeNode> {
    let actor = require_write(ctx)?;
    let ns = parse_namespace_id(namespace)?;
    require_namespace_access(ctx, actor, ns).await?;
    // The name must be valid in a bulk change spec.
    let parsed =
        sourcegull_batches::parse_batch_spec(&serde_json::json!({ "name": name }).to_string())
            .map_err(|_| {
                error(
                    batch_codes::INVALID_NAME,
                    "The bulk change name can only contain word characters, dots and dashes.",
                )
            })?;
    let db = &services(ctx)?.db;
    if let Some(existing) = BatchChangeStore::new(db)
        .get_by_name(ns, name)
        .await
        .map_err(db_error)?
    {
        if upsert {
            return Ok(BatchChangeNode(existing));
        }
        return Err(error(
            codes::ALREADY_EXISTS,
            "a bulk change with this name already exists in this namespace",
        ));
    }
    let spec = BatchSpecStore::new(db)
        .create(&NewBatchSpec {
            raw_spec: format!("name: {name}\n"),
            spec: serde_json::to_value(&parsed.spec).unwrap_or_default(),
            namespace: ns,
            user_id: Some(actor.user_id),
            created_from_raw: true,
            allow_unsupported: false,
            allow_ignored: false,
            no_cache: false,
            batch_change_id: None,
        })
        .await
        .map_err(db_error)?;
    let bc = BatchChangeStore::new(db)
        .create(&NewBatchChange {
            name: name.to_owned(),
            description: String::new(),
            namespace: ns,
            creator_id: Some(actor.user_id),
            batch_spec_id: spec.id,
            applied: false,
        })
        .await;
    match bc {
        Ok(bc) => Ok(BatchChangeNode(bc)),
        Err(e) => {
            // Lost a race for the name: drop the spec created for it.
            let _ = BatchSpecStore::new(db).delete(spec.id).await;
            if let (true, sourcegull_db::DbError::AlreadyExists { .. }) = (upsert, &e)
                && let Some(existing) = BatchChangeStore::new(db)
                    .get_by_name(ns, name)
                    .await
                    .map_err(db_error)?
            {
                return Ok(BatchChangeNode(existing));
            }
            Err(db_error(e))
        }
    }
}

async fn load_for_write(
    ctx: &Context<'_>,
    id: &str,
) -> Result<(BatchChange, sourcegull_authn::Actor)> {
    let actor = require_write(ctx)?.clone();
    let key = parse_batch_change_id(id)?;
    let bc = BatchChangeStore::new(&services(ctx)?.db)
        .get(key)
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change"))?;
    Ok((bc, actor))
}

/// `closeBatchChange`.
pub(super) async fn close(
    ctx: &Context<'_>,
    id: &str,
    close_changesets: bool,
) -> Result<BatchChangeNode> {
    let (bc, actor) = load_for_write(ctx, id).await?;
    if bc.closed_at.is_some() {
        return Ok(BatchChangeNode(bc));
    }
    require_can_modify(ctx, &actor, &bc).await?;
    let services = services(ctx)?;
    let closed = BatchChangeStore::new(&services.db)
        .close(bc.id, close_changesets)
        .await
        .map_err(db_error)?
        .ok_or_else(|| not_found("Bulk change"))?;
    webhook_events(services)
        .batch_change(sourcegull_outbound_webhooks::BATCH_CHANGE_CLOSE, &closed)
        .await;
    Ok(BatchChangeNode(closed))
}

/// Emits outbound webhook events.
pub(crate) fn webhook_events(
    services: &sourcegull_graphql_core::Services,
) -> sourcegull_outbound_webhooks::Events {
    sourcegull_outbound_webhooks::Events::new(
        services.db.clone(),
        services.keyring.clone(),
        services.config.clone(),
    )
}

/// `deleteBatchChange`.
pub(super) async fn delete(ctx: &Context<'_>, id: &str) -> Result<()> {
    let (bc, actor) = load_for_write(ctx, id).await?;
    require_can_modify(ctx, &actor, &bc).await?;
    let services = services(ctx)?;
    BatchChangeStore::new(&services.db)
        .delete(bc.id)
        .await
        .map_err(db_error)?;
    webhook_events(services)
        .batch_change(sourcegull_outbound_webhooks::BATCH_CHANGE_DELETE, &bc)
        .await;
    Ok(())
}
