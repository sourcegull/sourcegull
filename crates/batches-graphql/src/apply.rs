//! Preview and apply: `BatchSpec.applyPreview`, `applyBatchChange`
//! and `createBatchChange`.
//!
//! Both pair the bulk change spec's changeset specs with the changesets of the bulk change the spec
//! applies to (the one in its namespace with its name) through the rewirer
//! ([`sourcegull_batches_reconciler::rewire`]): new specs get new changesets, matched ones are
//! updated in place, and changesets no spec matches any more are archived (published ones the
//! bulk change created, closed first) or detached. The preview plans each changeset as the
//! apply would leave it with the reconciler's planner, so its operations are the ones the
//! reconciler then carries out. An apply runs in one transaction ([`sourcegull_batches::store::ApplyTx`]) and
//! queues the changesets it touched for the reconciler.
//!
//! `publicationStates` sets the publication of changeset specs that leave `published` unset
//! (the UI's choice); naming a spec that sets it, one not in the bulk change spec, or a spec twice is
//! an error.

use std::collections::{HashMap, HashSet};

use async_graphql::{Context, Enum, ID, InputObject, Result, SimpleObject, Union};
use sourcegull_authn::Actor;
use sourcegull_batches::store::{
    BatchChange, BatchChangeStore, BatchSpec, Changeset, ChangesetSpec, ChangesetSpecStore,
    ChangesetSpecType, ChangesetStore, ListChangesetSpecs, NewBatchChange, NewChangeset, Published,
    UiPublicationState,
};
use sourcegull_batches_reconciler::{Mapping, NEW_BATCH_CHANGE, Operation, Plan, Rewire, mappings};
use sourcegull_db::Db;
use sourcegull_types::{Repo, RepoId};

use super::batch_change::BatchChangeNode;
use super::batch_spec::{
    BatchSpecNode, BatchSpecStateGql, HiddenChangesetSpecNode, VisibleChangesetSpecNode, spec_by_id,
};
use super::changeset::{ExternalChangesetNode, HiddenExternalChangesetNode};
use super::{
    CHANGESET_SPEC_KIND, ChangesetStateGql, PublishedValue, audit, batch_codes, check_first,
    invalid, invalid_id, not_found, parse_batch_change_id, parse_string_id, readable_repos,
    require_namespace_access, require_write,
};
use crate::context::{db_error, error, services, to_int};
use crate::pagination::{PageInfo, decode_cursor};

/// Security event of an apply.
const BATCH_CHANGE_APPLIED: &str = "BatchChangeApplied";

/// The publication of one changeset spec chosen in the UI.
#[derive(InputObject)]
#[graphql(name = "ChangesetSpecPublicationStateInput")]
pub struct ChangesetSpecPublicationStateInput {
    /// The changeset spec (whose `published` must be unset).
    pub changeset_spec: ID,
    /// `true`, `false` or `"draft"`.
    pub publication_state: PublishedValue,
}

/// An operation the reconciler carries out on a changeset.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "ChangesetSpecOperation")]
pub enum ChangesetSpecOperationGql {
    #[graphql(name = "PUSH")]
    Push,
    #[graphql(name = "UPDATE")]
    Update,
    #[graphql(name = "UNDRAFT")]
    Undraft,
    #[graphql(name = "PUBLISH")]
    Publish,
    #[graphql(name = "PUBLISH_DRAFT")]
    PublishDraft,
    #[graphql(name = "SYNC")]
    Sync,
    #[graphql(name = "IMPORT")]
    Import,
    #[graphql(name = "CLOSE")]
    Close,
    #[graphql(name = "REOPEN")]
    Reopen,
    #[graphql(name = "SLEEP")]
    Sleep,
    #[graphql(name = "DETACH")]
    Detach,
    #[graphql(name = "ARCHIVE")]
    Archive,
    #[graphql(name = "REATTACH")]
    Reattach,
}

impl From<Operation> for ChangesetSpecOperationGql {
    fn from(op: Operation) -> Self {
        match op {
            Operation::Push => Self::Push,
            Operation::Update => Self::Update,
            Operation::Undraft => Self::Undraft,
            Operation::Publish => Self::Publish,
            Operation::PublishDraft => Self::PublishDraft,
            Operation::Sync => Self::Sync,
            Operation::Import => Self::Import,
            Operation::Close => Self::Close,
            Operation::Reopen => Self::Reopen,
            Operation::Sleep => Self::Sleep,
            Operation::Detach => Self::Detach,
            Operation::Archive => Self::Archive,
            Operation::Reattach => Self::Reattach,
        }
    }
}

/// What changed between the changeset's last spec and the new one.
#[derive(SimpleObject, Clone, Copy)]
#[graphql(name = "ChangesetSpecDelta")]
pub struct ChangesetSpecDelta {
    pub title_changed: bool,
    pub body_changed: bool,
    /// The spec went from `draft` to published.
    pub undraft: bool,
    pub base_ref_changed: bool,
    pub diff_changed: bool,
    pub commit_message_changed: bool,
    pub author_name_changed: bool,
    pub author_email_changed: bool,
}

impl From<sourcegull_batches_reconciler::Delta> for ChangesetSpecDelta {
    fn from(d: sourcegull_batches_reconciler::Delta) -> Self {
        Self {
            title_changed: d.title_changed,
            body_changed: d.body_changed,
            undraft: d.undraft,
            base_ref_changed: d.base_ref_changed,
            diff_changed: d.diff_changed,
            commit_message_changed: d.commit_message_changed,
            author_name_changed: d.author_name_changed,
            author_email_changed: d.author_email_changed,
        }
    }
}

/// A new changeset will be created for the spec.
#[derive(SimpleObject)]
pub struct VisibleApplyPreviewTargetsAttach {
    pub changeset_spec: VisibleChangesetSpecNode,
}

/// The changeset will be updated to the spec.
#[derive(SimpleObject)]
pub struct VisibleApplyPreviewTargetsUpdate {
    pub changeset_spec: VisibleChangesetSpecNode,
    pub changeset: ExternalChangesetNode,
}

/// No spec matches the changeset any more: it will be archived or detached.
#[derive(SimpleObject)]
pub struct VisibleApplyPreviewTargetsDetach {
    pub changeset: ExternalChangesetNode,
}

/// What a visible preview entry applies to.
#[derive(Union)]
pub enum VisibleApplyPreviewTargets {
    Attach(VisibleApplyPreviewTargetsAttach),
    Update(VisibleApplyPreviewTargetsUpdate),
    Detach(VisibleApplyPreviewTargetsDetach),
}

/// A new changeset will be created for the (hidden) spec.
#[derive(SimpleObject)]
pub struct HiddenApplyPreviewTargetsAttach {
    pub changeset_spec: HiddenChangesetSpecNode,
}

/// The (hidden) changeset will be updated to the spec.
#[derive(SimpleObject)]
pub struct HiddenApplyPreviewTargetsUpdate {
    pub changeset_spec: HiddenChangesetSpecNode,
    pub changeset: HiddenExternalChangesetNode,
}

/// The (hidden) changeset will be archived or detached.
#[derive(SimpleObject)]
pub struct HiddenApplyPreviewTargetsDetach {
    pub changeset: HiddenExternalChangesetNode,
}

/// What a hidden preview entry applies to.
#[derive(Union)]
pub enum HiddenApplyPreviewTargets {
    Attach(HiddenApplyPreviewTargetsAttach),
    Update(HiddenApplyPreviewTargetsUpdate),
    Detach(HiddenApplyPreviewTargetsDetach),
}

/// The preview of one changeset in a repository the viewer may read.
#[derive(SimpleObject)]
pub struct VisibleChangesetApplyPreview {
    /// What the reconciler will do after the apply, in order.
    pub operations: Vec<ChangesetSpecOperationGql>,
    pub delta: ChangesetSpecDelta,
    pub targets: VisibleApplyPreviewTargets,
}

/// The preview of one changeset in a repository the viewer may not read.
#[derive(SimpleObject)]
pub struct HiddenChangesetApplyPreview {
    pub operations: Vec<ChangesetSpecOperationGql>,
    pub delta: ChangesetSpecDelta,
    pub targets: HiddenApplyPreviewTargets,
}

/// One entry of an apply preview.
#[derive(Union)]
pub enum ChangesetApplyPreview {
    VisibleChangesetApplyPreview(VisibleChangesetApplyPreview),
    HiddenChangesetApplyPreview(HiddenChangesetApplyPreview),
}

/// Counts over the whole preview (filters and pages aside).
#[derive(SimpleObject, Default)]
pub struct ChangesetApplyPreviewConnectionStats {
    pub push: i32,
    pub update: i32,
    pub undraft: i32,
    pub publish: i32,
    pub publish_draft: i32,
    pub sync: i32,
    pub import: i32,
    pub close: i32,
    pub reopen: i32,
    pub sleep: i32,
    pub detach: i32,
    pub archive: i32,
    pub reattach: i32,
    /// New changesets.
    pub added: i32,
    /// Changesets that get operations.
    pub modified: i32,
    /// Changesets that leave the bulk change.
    pub removed: i32,
}

impl ChangesetApplyPreviewConnectionStats {
    fn add(&mut self, m: &Mapping, plan: &Plan) {
        for op in &plan.ops {
            let slot = match op {
                Operation::Push => &mut self.push,
                Operation::Update => &mut self.update,
                Operation::Undraft => &mut self.undraft,
                Operation::Publish => &mut self.publish,
                Operation::PublishDraft => &mut self.publish_draft,
                Operation::Sync => &mut self.sync,
                Operation::Import => &mut self.import,
                Operation::Close => &mut self.close,
                Operation::Reopen => &mut self.reopen,
                Operation::Sleep => &mut self.sleep,
                Operation::Detach => &mut self.detach,
                Operation::Archive => &mut self.archive,
                Operation::Reattach => &mut self.reattach,
            };
            *slot += 1;
        }
        match (&m.spec, &m.changeset) {
            (Some(_), None) => self.added += 1,
            (None, Some(_)) => self.removed += 1,
            _ if !plan.is_empty() => self.modified += 1,
            _ => {}
        }
    }
}

/// A page of an apply preview.
#[derive(SimpleObject)]
pub struct ChangesetApplyPreviewConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<ChangesetApplyPreview>,
    pub stats: ChangesetApplyPreviewConnectionStats,
}

/// Filters of `applyPreview`.
pub struct PreviewArgs {
    pub first: Option<i32>,
    pub after: Option<String>,
    pub search: Option<String>,
    pub current_state: Option<ChangesetStateGql>,
    pub action: Option<ChangesetSpecOperationGql>,
    pub publication_states: Option<Vec<ChangesetSpecPublicationStateInput>>,
}

/// The bulk change spec's name (from the spec).
fn spec_name(spec: &BatchSpec) -> &str {
    spec.spec
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// The UI publication states of `input` by changeset spec id, checked against `specs`.
fn publication_states(
    specs: &[ChangesetSpec],
    input: Option<&[ChangesetSpecPublicationStateInput]>,
) -> Result<HashMap<i64, UiPublicationState>> {
    let mut out = HashMap::new();
    let by_rand: HashMap<&str, &ChangesetSpec> =
        specs.iter().map(|s| (s.rand_id.as_str(), s)).collect();
    for i in input.unwrap_or_default() {
        let rand = parse_string_id(CHANGESET_SPEC_KIND, &i.changeset_spec)
            .ok_or_else(|| invalid_id(&i.changeset_spec))?;
        let Some(spec) = by_rand.get(rand.as_str()) else {
            return Err(invalid(format!(
                "changeset spec {} not found in the bulk change spec",
                i.changeset_spec.as_str()
            )));
        };
        if spec.kind != ChangesetSpecType::Branch || spec.published.is_some() {
            return Err(invalid(format!(
                "changeset spec {} has the published field set in its spec",
                i.changeset_spec.as_str()
            )));
        }
        let state = match i.publication_state.0 {
            Published::Bool(true) => UiPublicationState::Published,
            Published::Bool(false) => UiPublicationState::Unpublished,
            Published::Draft => UiPublicationState::Draft,
        };
        if out.insert(spec.id, state).is_some() {
            return Err(invalid(format!(
                "duplicate changeset spec {} in publication states",
                i.changeset_spec.as_str()
            )));
        }
    }
    Ok(out)
}

/// The bulk change spec's changeset specs, in id order.
async fn spec_changeset_specs(db: &Db, batch_spec_id: i64) -> Result<Vec<ChangesetSpec>> {
    ChangesetSpecStore::new(db)
        .list(&ListChangesetSpecs {
            batch_spec_id: Some(batch_spec_id),
            ..Default::default()
        })
        .await
        .map_err(db_error)
}

/// The (repository, external id) pairs of the import specs.
fn imports(specs: &[ChangesetSpec]) -> Vec<(RepoId, String)> {
    specs
        .iter()
        .filter(|s| s.kind == ChangesetSpecType::Existing)
        .filter_map(|s| Some((s.repo_id, s.external_id.clone()?)))
        .collect()
}

/// The current and previous specs of `changesets`, by id.
async fn changeset_specs_of(
    db: &Db,
    changesets: &[Changeset],
) -> Result<HashMap<i64, ChangesetSpec>> {
    let ids: Vec<i64> = changesets
        .iter()
        .flat_map(|c| [c.current_spec_id, c.previous_spec_id])
        .flatten()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let specs = ChangesetSpecStore::new(db)
        .list(&ListChangesetSpecs {
            ids: Some(ids),
            ..Default::default()
        })
        .await
        .map_err(db_error)?;
    Ok(specs.into_iter().map(|s| (s.id, s)).collect())
}

/// The code host type of repositories by id, whether or not the viewer may read them (only
/// used to plan; nothing about them is shown).
async fn service_types(db: &Db, ids: &[RepoId]) -> Result<HashMap<RepoId, String>> {
    let ids: Vec<i32> = ids.iter().map(|r| r.get()).collect();
    let rows: Vec<(i32, String)> = sourcegull_db::sqlx::query_as(
        "SELECT id, external_service_type FROM repo WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(db.pool())
    .await
    .map_err(|e| db_error(e.into()))?;
    Ok(rows.into_iter().map(|(id, t)| (RepoId(id), t)).collect())
}

/// The bulk change the spec applies to: the one created for it (server-side specs), else
/// the one in its namespace with its name.
async fn target_batch_change(db: &Db, spec: &BatchSpec) -> Result<Option<BatchChange>> {
    if let Some(id) = spec.batch_change_id {
        return BatchChangeStore::new(db).get(id).await.map_err(db_error);
    }
    BatchChangeStore::new(db)
        .get_by_name(spec.namespace, spec_name(spec))
        .await
        .map_err(db_error)
}

/// Whether `m` matches the preview's text search (every term in the title or repository
/// name, none of the `-term`s).
fn matches_search(m: &Mapping, repo: &Repo, q: &str) -> bool {
    let title = m
        .spec
        .as_ref()
        .and_then(|s| s.title.as_deref())
        .or_else(|| m.changeset.as_ref().and_then(|c| c.title.as_deref()))
        .unwrap_or_default()
        .to_lowercase();
    let name = repo.name.to_lowercase();
    q.split_whitespace().all(|term| {
        let (neg, term) = match term.strip_prefix('-') {
            Some(t) if !t.is_empty() => (true, t),
            _ => (false, term),
        };
        let term = term.to_lowercase();
        let hit = title.contains(&term) || name.contains(&term);
        hit != neg
    })
}

/// `BatchSpec.applyPreview`.
pub(super) async fn preview(
    ctx: &Context<'_>,
    spec: &BatchSpec,
    args: PreviewArgs,
) -> Result<ChangesetApplyPreviewConnection> {
    let limit = check_first(args.first)?;
    let offset = decode_cursor(args.after.as_deref())?;
    let db = &services(ctx)?.db;
    let specs = spec_changeset_specs(db, spec.id).await?;
    let ui = publication_states(&specs, args.publication_states.as_deref())?;
    let bc = target_batch_change(db, spec).await?;
    let bc_id = bc.as_ref().map(|b| b.id);
    let candidates = ChangesetStore::new(db)
        .mapping_candidates(bc_id, &imports(&specs))
        .await
        .map_err(db_error)?;
    let changeset_specs = changeset_specs_of(db, &candidates).await?;
    let all = mappings(bc_id, specs, candidates, &changeset_specs);

    let repo_ids: Vec<RepoId> = all.iter().map(Mapping::repo_id).collect();
    let readable = readable_repos(ctx, &repo_ids).await?;
    let types = service_types(db, &repo_ids).await?;
    let target = bc_id.unwrap_or(NEW_BATCH_CHANGE);
    let search = args
        .search
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty());

    let mut stats = ChangesetApplyPreviewConnectionStats::default();
    let mut entries = Vec::new();
    for m in all {
        let repo_id = m.repo_id();
        let ui_state = m.spec.as_ref().and_then(|s| ui.get(&s.id)).copied();
        let service_type = types.get(&repo_id).map_or("", String::as_str);
        let plan = sourcegull_batches_reconciler::preview(
            &m,
            target,
            service_type,
            ui_state,
            &changeset_specs,
        );
        stats.add(&m, &plan);
        let repo = readable.get(&repo_id);
        if let Some(q) = search {
            // Text could tell something about a hidden changeset: those are left out.
            if !repo.is_some_and(|r| matches_search(&m, r, q)) {
                continue;
            }
        }
        if let Some(state) = args.current_state {
            let current = m
                .changeset
                .as_ref()
                .map(|c| ChangesetStateGql::from(c.state));
            if current != Some(state) {
                continue;
            }
        }
        if let Some(action) = args.action
            && !plan
                .ops
                .iter()
                .any(|op| ChangesetSpecOperationGql::from(*op) == action)
        {
            continue;
        }
        entries.push((m, plan, repo.cloned()));
    }

    let total = entries.len();
    let skip = usize::try_from(offset).unwrap_or(usize::MAX);
    let take = usize::try_from(limit).unwrap_or(0);
    let fetched = total.saturating_sub(skip).min(take + 1);
    let nodes = entries
        .into_iter()
        .skip(skip)
        .take(take)
        .filter_map(|(m, plan, repo)| preview_node(m, &plan, repo))
        .collect();
    Ok(ChangesetApplyPreviewConnection {
        total_count: to_int(i64::try_from(total).unwrap_or(i64::MAX)),
        page_info: PageInfo::new(offset, limit, fetched),
        nodes,
        stats,
    })
}

/// The node of one preview entry; hidden without `repo`.
fn preview_node(m: Mapping, plan: &Plan, repo: Option<Repo>) -> Option<ChangesetApplyPreview> {
    let operations = plan.ops.iter().copied().map(Into::into).collect();
    let delta = plan.delta.into();
    Some(match repo {
        Some(repo) => {
            let targets = match (m.spec, m.changeset) {
                (Some(s), None) => {
                    VisibleApplyPreviewTargets::Attach(VisibleApplyPreviewTargetsAttach {
                        changeset_spec: VisibleChangesetSpecNode::new(s, repo),
                    })
                }
                (Some(s), Some(c)) => {
                    VisibleApplyPreviewTargets::Update(VisibleApplyPreviewTargetsUpdate {
                        changeset_spec: VisibleChangesetSpecNode::new(s, repo.clone()),
                        changeset: ExternalChangesetNode::new(c, repo),
                    })
                }
                (None, Some(c)) => {
                    VisibleApplyPreviewTargets::Detach(VisibleApplyPreviewTargetsDetach {
                        changeset: ExternalChangesetNode::new(c, repo),
                    })
                }
                (None, None) => return None,
            };
            ChangesetApplyPreview::VisibleChangesetApplyPreview(VisibleChangesetApplyPreview {
                operations,
                delta,
                targets,
            })
        }
        None => {
            let targets = match (m.spec, m.changeset) {
                (Some(s), None) => {
                    HiddenApplyPreviewTargets::Attach(HiddenApplyPreviewTargetsAttach {
                        changeset_spec: HiddenChangesetSpecNode::new(s),
                    })
                }
                (Some(s), Some(c)) => {
                    HiddenApplyPreviewTargets::Update(HiddenApplyPreviewTargetsUpdate {
                        changeset_spec: HiddenChangesetSpecNode::new(s),
                        changeset: HiddenExternalChangesetNode::new(c),
                    })
                }
                (None, Some(c)) => {
                    HiddenApplyPreviewTargets::Detach(HiddenApplyPreviewTargetsDetach {
                        changeset: HiddenExternalChangesetNode::new(c),
                    })
                }
                (None, None) => return None,
            };
            ChangesetApplyPreview::HiddenChangesetApplyPreview(HiddenChangesetApplyPreview {
                operations,
                delta,
                targets,
            })
        }
    })
}

/// Options of an apply.
pub(super) struct ApplyOpts<'a> {
    pub batch_spec: &'a str,
    /// `ensureBatchChange`: the bulk change the apply must update.
    pub ensure: Option<&'a str>,
    pub publication_states: Option<Vec<ChangesetSpecPublicationStateInput>>,
    /// `createBatchChange`: fail if the bulk change exists.
    pub create_only: bool,
}

/// `applyBatchChange` / `createBatchChange`.
pub(super) async fn apply(ctx: &Context<'_>, opts: ApplyOpts<'_>) -> Result<BatchChangeNode> {
    let actor = require_write(ctx)?;
    let spec = spec_by_id(ctx, opts.batch_spec).await?;
    require_namespace_access(ctx, actor, spec.namespace).await?;
    let ensure = opts.ensure.map(parse_batch_change_id).transpose()?;
    let node = BatchSpecNode::new(spec.clone());
    if node.compute_state(ctx).await? != BatchSpecStateGql::Completed {
        return Err(invalid(
            "The bulk change spec has not finished executing and cannot be applied yet.",
        ));
    }
    let db = &services(ctx)?.db;
    let specs = spec_changeset_specs(db, spec.id).await?;
    let ui = publication_states(&specs, opts.publication_states.as_deref())?;
    // The applier must be able to read every repository the bulk change spec changes.
    let spec_repos: Vec<RepoId> = specs.iter().map(|s| s.repo_id).collect();
    let readable = readable_repos(ctx, &spec_repos).await?;
    if spec_repos.iter().any(|r| !readable.contains_key(r)) {
        return Err(not_found("Repository"));
    }

    let mut tx = BatchChangeStore::new(db)
        .begin_apply()
        .await
        .map_err(db_error)?;
    let existing = tx
        .lock_batch_change(spec.namespace, spec_name(&spec))
        .await
        .map_err(db_error)?;
    check_target(&spec, existing.as_ref(), ensure, opts.create_only)?;
    if let Some(bc) = &existing
        && bc.batch_spec_id == spec.id
        && bc.last_applied_at.is_some()
    {
        // Already applied: nothing to do.
        return Ok(BatchChangeNode::new(bc.clone()));
    }
    let created = existing.is_none();
    let bc = match existing {
        Some(bc) => bc,
        None => tx
            .create_batch_change(&NewBatchChange {
                name: spec_name(&spec).to_owned(),
                description: String::new(),
                namespace: spec.namespace,
                creator_id: Some(actor.user_id),
                batch_spec_id: spec.id,
                applied: true,
            })
            .await
            .map_err(|e| match e {
                sourcegull_db::DbError::AlreadyExists { .. } => matching_exists(),
                e => db_error(e),
            })?,
    };
    let description = spec
        .spec
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let bc = tx
        .record_apply(bc.id, spec.id, description, actor.user_id)
        .await
        .map_err(db_error)?;

    let candidates = tx
        .mapping_candidates(Some(bc.id), &imports(&specs))
        .await
        .map_err(db_error)?;
    let current = changeset_specs_of(db, &candidates).await?;
    let all = mappings(Some(bc.id), specs, candidates, &current);
    let types = service_types(db, &spec_repos).await?;
    let mut attach = Vec::new();
    let mut detach = Vec::new();
    let mut archive = Vec::new();
    let mut enqueue = Vec::new();
    for m in &all {
        let ui_state = m.spec.as_ref().and_then(|s| ui.get(&s.id)).copied();
        match (Rewire::of(m, bc.id), &m.spec, &m.changeset) {
            (Rewire::CreateBranch, Some(s), _) => {
                let mut new = NewChangeset::new(s.repo_id, service_type(&types, s.repo_id));
                new.batch_change_id = Some(bc.id);
                new.owned_by_batch_change_id = Some(bc.id);
                new.current_spec_id = Some(s.id);
                new.ui_publication_state = ui_state;
                tx.create_changeset(&new).await.map_err(db_error)?;
            }
            (Rewire::CreateImport, Some(s), _) => {
                let mut new = NewChangeset::new(s.repo_id, service_type(&types, s.repo_id));
                new.batch_change_id = Some(bc.id);
                new.external_id = s.external_id.clone();
                tx.create_changeset(&new).await.map_err(db_error)?;
            }
            (Rewire::Update, Some(s), Some(c)) => {
                tx.set_changeset_spec(c.id, s.id).await.map_err(db_error)?;
                if ui_state.is_some() {
                    tx.set_ui_publication_state(c.id, ui_state)
                        .await
                        .map_err(db_error)?;
                }
                attach.push(c.id);
                enqueue.push(c.id);
            }
            (Rewire::Attach { enqueue: requeue }, _, Some(c)) => {
                attach.push(c.id);
                if requeue {
                    enqueue.push(c.id);
                }
            }
            (Rewire::Archive, _, Some(c)) => {
                archive.push(c.id);
                enqueue.push(c.id);
            }
            (Rewire::Detach, _, Some(c)) => {
                detach.push(c.id);
                enqueue.push(c.id);
            }
            _ => {}
        }
    }
    tx.attach(bc.id, &attach).await.map_err(db_error)?;
    tx.archive(bc.id, &archive, true).await.map_err(db_error)?;
    tx.detach(bc.id, &detach).await.map_err(db_error)?;
    tx.enqueue(&enqueue, false).await.map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    record(ctx, actor, &bc, &spec, created, &all).await;
    super::batch_change::webhook_events(services(ctx)?)
        .batch_change(sourcegull_outbound_webhooks::BATCH_CHANGE_APPLY, &bc)
        .await;
    Ok(BatchChangeNode::new(bc))
}

fn service_type(types: &HashMap<RepoId, String>, repo: RepoId) -> String {
    types.get(&repo).cloned().unwrap_or_default()
}

fn matching_exists() -> async_graphql::Error {
    error(
        batch_codes::MATCHING_BATCH_CHANGE_EXISTS,
        "a bulk change matching the given bulk change spec already exists",
    )
}

/// The checks of the bulk change an apply targets (`existing`, by namespace and name).
fn check_target(
    spec: &BatchSpec,
    existing: Option<&BatchChange>,
    ensure: Option<i64>,
    create_only: bool,
) -> Result<()> {
    if create_only && existing.is_some() {
        return Err(matching_exists());
    }
    if let Some(want) = ensure
        && existing.map(|b| b.id) != Some(want)
    {
        return Err(error(
            batch_codes::ENSURE_BATCH_CHANGE_FAILED,
            "a bulk change in the given namespace and with the given name exists but does not \
             match the given ID",
        ));
    }
    if let Some(owner) = spec.batch_change_id
        && existing.map(|b| b.id) != Some(owner)
    {
        return Err(invalid(
            "The bulk change spec was created for another bulk change and cannot be applied to this \
             one.",
        ));
    }
    if existing.is_some_and(|b| b.closed_at.is_some()) {
        return Err(error(
            batch_codes::APPLY_CLOSED_BATCH_CHANGE,
            "existing bulk change matched by bulk change spec is closed",
        ));
    }
    Ok(())
}

/// Records the apply as a security event.
async fn record(
    ctx: &Context<'_>,
    actor: &Actor,
    bc: &BatchChange,
    spec: &BatchSpec,
    created: bool,
    all: &[Mapping],
) {
    let count = |f: fn(&Mapping) -> bool| all.iter().filter(|m| f(m)).count();
    audit(
        ctx,
        BATCH_CHANGE_APPLIED,
        actor,
        serde_json::json!({
            "batchChangeID": bc.id,
            "batchSpecID": spec.rand_id,
            "created": created,
            "added": count(|m| m.changeset.is_none()),
            "updated": count(|m| m.spec.is_some() && m.changeset.is_some()),
            "removed": count(|m| m.spec.is_none()),
        }),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_map_one_to_one() {
        use async_graphql::{InputType, Name, Value};
        for op in Operation::ALL {
            let gql = ChangesetSpecOperationGql::from(*op);
            assert_eq!(gql.to_value(), Value::Enum(Name::new(op.as_str())), "{op}");
        }
    }
}
